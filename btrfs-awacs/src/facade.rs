//! Synchronized immutable-scan facade for direct AWACS clients.

use crate::compat::{
    BoundaryKind, CursorClaims, Projection, encode_direct_scan_cursor, project_events,
};
use crate::manager::{FacadeActivation, QueryLeaseReservation};
use crate::namespace::NamespaceMonitor;
use crate::namespace::ViewBinding;
use crate::service::{ChangesOptions, Service};
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const ALGORITHM_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResult {
    pub cursor: Vec<u8>,
    pub projection: Projection,
    pub sequence: i64,
}

/// A projected scan whose immutable inputs remain pinned until the client
/// commits or aborts its lease. The fields which authorize release stay
/// private so a caller cannot manufacture or retarget a response fence.
pub struct PreparedQueryResult {
    pub result: QueryResult,
    lease: QueryLeaseReservation,
    activation: FacadeActivation,
    snapshot_id: i64,
}

struct ActiveView {
    authorization_id: [u8; 16],
    activation: FacadeActivation,
    monitor: NamespaceMonitor,
}

pub struct FacadeService {
    service: Service,
    views: BTreeMap<[u8; 16], ActiveView>,
    query_lease_owner: [u8; 16],
}

struct PreparedScanInput {
    activation: FacadeActivation,
    authorization_id: [u8; 16],
    published: crate::manager::PublishedCut,
    previous_snapshot_uuid: Option<[u8; 16]>,
    requester_uid: u32,
    lease_expires_ns: i64,
}

impl FacadeService {
    pub fn new(service: Service) -> Self {
        Self {
            service,
            views: BTreeMap::new(),
            query_lease_owner: *Uuid::new_v4().as_bytes(),
        }
    }

    pub fn service(&self) -> &Service {
        &self.service
    }

    pub fn service_mut(&mut self) -> &mut Service {
        &mut self.service
    }

    /// Returns the immutable sequence-zero baseline for a newly initialized
    /// direct-scan consumer.
    ///
    /// Git has no AWACS token before its first status in a new worktree. That
    /// is still an exact state: descendant initialization published snapshot
    /// A as sequence zero before Git materialized B. Bootstrapping from A lets
    /// the first Git query receive an exact A -> B delta and a real token
    /// without manufacturing a whole-tree invalidation.
    pub fn initial_scan_baseline(
        &self,
        watch_id: [u8; 16],
    ) -> Result<crate::scan::SnapshotBaseline, FacadeError> {
        let (filesystem_uuid, subvolume_uuid, read_only): (Vec<u8>, Vec<u8>, i64) = self
            .service
            .store()
            .connection()
            .query_row(
                r#"SELECT f.fs_uuid, s.subvol_uuid, s.readonly
                     FROM watches w
                     JOIN snapshots s ON s.id = COALESCE(
                         (
                             SELECT c.base_snapshot_id
                               FROM watch_cuts c
                              WHERE c.watch_id = w.id
                              ORDER BY c.sequence
                              LIMIT 1
                         ),
                         w.last_cut_snapshot_id
                     )
                     JOIN filesystems f ON f.id = s.filesystem_id
                    WHERE w.id = ?1 AND w.state = 'active'"#,
                [watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| FacadeError::context("load initial scan baseline", error))?;
        if read_only != 1 {
            return Err(FacadeError::new(
                "initial scan baseline is not an immutable snapshot",
            ));
        }
        Ok(crate::scan::SnapshotBaseline {
            identity: crate::scan::SnapshotIdentity {
                filesystem_uuid: filesystem_uuid
                    .as_slice()
                    .try_into()
                    .map_err(|_| FacadeError::new("initial baseline filesystem UUID is invalid"))?,
                subvolume_uuid: subvolume_uuid
                    .as_slice()
                    .try_into()
                    .map_err(|_| FacadeError::new("initial baseline subvolume UUID is invalid"))?,
                read_only: true,
            },
            // The first consumer has no prior cursor to authenticate. The
            // immutable identity above is sufficient to select sequence zero;
            // the response supplies the first authenticated continuity token.
            continuity_token: Vec::new(),
            // One initialized watch is one independent Git fsmonitor lane.
            // Carry its stable owner id through the first tokenless query so
            // the normal consumer-baseline pin machinery can retain Git's
            // endpoint independently from JJ's workspace owner.
            retention_token: watch_id.to_vec(),
        })
    }

    pub fn view_binding(&self, watch_id: [u8; 16]) -> Option<&ViewBinding> {
        self.views.get(&watch_id).map(|view| view.monitor.binding())
    }

    /// Drops a stale in-memory namespace binding before a durable watch is
    /// rebound to the same subvolume at a renamed path.
    pub fn forget_view(&mut self, watch_id: [u8; 16]) {
        self.views.remove(&watch_id);
    }

    pub fn activate(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        root: &Path,
    ) -> Result<(), FacadeError> {
        if !self
            .service
            .ensure_snapshot_facade_is_enabled(watch_id)
            .map_err(|error| FacadeError::context("verify dirty-witness ABI", error))?
        {
            return Err(FacadeError::new(
                "snapshot facade is disabled until the experimental dirty-witness ABI is verified",
            ));
        }
        let activation_started = std::time::Instant::now();
        let monitor_arm_started = std::time::Instant::now();
        let monitor = NamespaceMonitor::arm(root)
            .map_err(|error| FacadeError::context("arm namespace monitor", error))?;
        tracing::info!(
            elapsed_ms = monitor_arm_started.elapsed().as_millis() as u64,
            root = %root.display(),
            "facade activation armed namespace monitor"
        );
        let continuity_started = std::time::Instant::now();
        monitor
            .check_continuity()
            .map_err(|error| FacadeError::context("validate namespace monitor", error))?;
        tracing::info!(
            elapsed_ms = continuity_started.elapsed().as_millis() as u64,
            root = %root.display(),
            "facade activation checked namespace continuity"
        );
        let store_activation_started = std::time::Instant::now();
        let activation = self
            .service
            .store_mut()
            .activate_snapshot_facade(watch_id, authorization_id, monitor.binding())
            .map_err(|error| FacadeError::context("activate facade in store", error))?;
        tracing::info!(
            elapsed_ms = store_activation_started.elapsed().as_millis() as u64,
            root = %root.display(),
            "facade activation committed store binding"
        );
        self.views.insert(
            watch_id,
            ActiveView {
                authorization_id,
                activation,
                monitor,
            },
        );
        tracing::info!(
            elapsed_ms = activation_started.elapsed().as_millis() as u64,
            root = %root.display(),
            "facade activation completed"
        );
        Ok(())
    }

    /// Builds a direct-scan response whose durable lease pins only its target
    /// snapshot.
    ///
    /// Previous cursors are resolved through the adjacent event journal, not
    /// by reopening or pinning their physical snapshot. The longer-lived scan
    /// lease therefore never retains the whole interval between the prior and
    /// selected cuts.
    pub fn prepare_scan_query(
        &mut self,
        watch_id: [u8; 16],
        previous_baseline: Option<&crate::scan::SnapshotBaseline>,
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
        lease_expires_ns: i64,
    ) -> Result<PreparedQueryResult, FacadeError> {
        self.check_continuity_or_invalidate(watch_id, "pre-cut namespace check")?;
        let view = self.views.get(&watch_id).expect("checked active view");
        let authorization_id = view.authorization_id;
        let activation = view.activation.clone();
        let published = self
            .service
            .changes(&ChangesOptions {
                watch_id,
                authorization_id,
                requester_uid,
                requester_gid,
                now_ns,
            })
            .map_err(|error| FacadeError::context("take synchronized scan cut", error))?;
        // The opaque cursor authenticates this exact immutable snapshot
        // identity. Direct mode can resolve its logical cut from the durable
        // journal even after the old snapshot pathname has been collected.
        let previous_snapshot_uuid = previous_baseline
            .filter(|baseline| baseline.identity.read_only)
            .map(|baseline| baseline.identity.subvolume_uuid);
        self.prepare_scan_after_cut(PreparedScanInput {
            activation,
            authorization_id,
            published,
            previous_snapshot_uuid,
            requester_uid,
            lease_expires_ns,
        })
    }

    /// Reconciles one consumer's durable baseline pins with the snapshot
    /// identity durably named by its local journal.
    pub fn reconcile_consumer_baseline(
        &mut self,
        watch_id: [u8; 16],
        owner_id: [u8; 16],
        previous_baseline: Option<&crate::scan::SnapshotBaseline>,
    ) -> Result<bool, FacadeError> {
        let authorization_id = self
            .views
            .get(&watch_id)
            .ok_or_else(|| FacadeError::new("scan facade is not active"))?
            .authorization_id;
        let previous_snapshot_id = previous_baseline
            .map(|baseline| self.snapshot_id_for_uuid(baseline.identity.subvolume_uuid))
            .transpose()?
            .flatten();
        self.service
            .store_mut()
            .reconcile_consumer_baseline(watch_id, authorization_id, owner_id, previous_snapshot_id)
            .map_err(|error| FacadeError::context("reconcile consumer baseline", error))
    }

    /// Pins a candidate B while leaving the committed A pin intact.
    pub fn stage_consumer_baseline(
        &mut self,
        prepared: &PreparedQueryResult,
        owner_id: [u8; 16],
    ) -> Result<(), FacadeError> {
        self.service
            .store_mut()
            .stage_consumer_baseline(
                prepared.activation.watch_id,
                prepared.activation.authorization_id,
                owner_id,
                prepared.snapshot_id,
            )
            .map_err(|error| FacadeError::context("stage consumer baseline", error))
    }

    /// Finalizes or aborts the candidate pin after the local journal commit.
    pub fn finish_consumer_baseline(
        &mut self,
        owner_id: [u8; 16],
        committed: bool,
    ) -> Result<(), FacadeError> {
        self.service
            .store_mut()
            .finish_consumer_baseline(owner_id, committed)
            .map_err(|error| FacadeError::context("finish consumer baseline", error))
    }

    /// Drops every committed or pending baseline pin owned by one consumer.
    pub fn release_consumer_baseline(&mut self, owner_id: [u8; 16]) -> Result<(), FacadeError> {
        self.service
            .store_mut()
            .release_consumer_baseline(owner_id)
            .map_err(|error| FacadeError::context("release consumer baseline", error))
    }

    /// Returns the opaque continuity token prepared for the direct scan API.
    pub fn direct_scan_continuity_token(
        &self,
        prepared: &PreparedQueryResult,
    ) -> Result<Vec<u8>, FacadeError> {
        Ok(prepared.result.cursor.clone())
    }

    fn prepare_scan_after_cut(
        &mut self,
        input: PreparedScanInput,
    ) -> Result<PreparedQueryResult, FacadeError> {
        let PreparedScanInput {
            activation,
            authorization_id,
            published,
            previous_snapshot_uuid,
            requester_uid,
            lease_expires_ns,
        } = input;
        let watch_id = activation.watch_id;
        self.check_continuity_or_invalidate(watch_id, "post-cut namespace check")?;
        let view = self.views.get(&watch_id).expect("checked active view");
        self.service
            .store_mut()
            .finalize_cut_boundary(
                &activation,
                view.monitor.binding(),
                published.sequence,
                None,
            )
            .map_err(|error| FacadeError::context("finalize scan boundary", error))?;
        self.check_continuity_or_invalidate(watch_id, "pre-response namespace check")?;

        let metadata = self
            .service
            .store()
            .metadata()
            .map_err(|error| FacadeError::context("load cursor metadata", error))?;
        let snapshot_uuid = self.snapshot_uuid(published.snapshot_id)?;
        let claims = CursorClaims {
            format_version: metadata.clock_format_version,
            store_uuid: metadata.store_uuid,
            watch_id,
            cursor_epoch: activation.clock_epoch,
            cut_sequence: u64::try_from(published.sequence)
                .map_err(|_| FacadeError::new("negative cut sequence"))?,
            owner_grant_id: authorization_id,
            monitor_session_id: activation.monitor_session_id,
            boundary_kind: BoundaryKind::Cut,
            algorithm_version: ALGORITHM_VERSION,
            target_snapshot_uuid: snapshot_uuid,
        };
        let lease = self
            .service
            .store_mut()
            .begin_query_lease(
                &activation,
                None,
                published.sequence,
                self.query_lease_owner,
                lease_expires_ns,
            )
            .map_err(|error| FacadeError::context("pin scan inputs", error))?;
        let projection = if let Some(from_snapshot_uuid) = previous_snapshot_uuid {
            // Cursor replay is intentionally based on the durable adjacent
            // event journal rather than a fresh historical snapshot
            // comparison. That lets completed scans release old physical
            // snapshots while retained cursors still receive an exact union
            // of every intervening cut.
            match self.service.store_mut().replay_historical_changes(
                watch_id,
                authorization_id,
                requester_uid,
                from_snapshot_uuid,
                snapshot_uuid,
            ) {
                Ok(changes) if !changes.fresh_instance => {
                    let projection = project_events(&changes.events);
                    tracing::debug!(
                        event_count = changes.events.len(),
                        projected_path_count = projection.paths.len(),
                        projected_prefix_count = projection.prefixes.len(),
                        fresh_instance = projection.fresh_instance,
                        "direct retained-event replay projected delta"
                    );
                    projection
                }
                Ok(changes) => {
                    let release = self
                        .service
                        .store_mut()
                        .release_query_lease(&lease, &activation);
                    let mut message = format!(
                        "direct retained-event replay cannot prove an exact delta from sequence {} to {}",
                        changes.from_sequence, changes.to_sequence
                    );
                    if let Err(release) = release {
                        message.push_str(&format!("; scan lease release failed: {release}"));
                    }
                    return Err(FacadeError::new(message));
                }
                Err(error) => {
                    let release = self
                        .service
                        .store_mut()
                        .release_query_lease(&lease, &activation);
                    let mut message = format!(
                        "direct retained-event replay could not prove an exact delta from {} to {}: {error}",
                        Uuid::from_bytes(from_snapshot_uuid),
                        Uuid::from_bytes(snapshot_uuid)
                    );
                    if let Err(release) = release {
                        message.push_str(&format!("; scan lease release failed: {release}"));
                    }
                    return Err(FacadeError::new(message));
                }
            }
        } else {
            Projection {
                fresh_instance: true,
                paths: vec![b"/".to_vec()],
                prefixes: Vec::new(),
            }
        };
        let cursor = encode_direct_scan_cursor(&claims, &metadata.clock_hmac_key);
        if let Err(continuity) =
            self.check_continuity_or_invalidate(watch_id, "final response namespace check")
        {
            let release = self
                .service
                .store_mut()
                .release_query_lease(&lease, &activation);
            let invalidation = match &release {
                Ok(()) => {
                    let result = self
                        .service
                        .store_mut()
                        .invalidate_snapshot_facade(&activation);
                    if result.is_ok() {
                        self.views.remove(&watch_id);
                    }
                    result.err()
                }
                Err(_) => None,
            };
            let mut message = continuity.to_string();
            if let Err(release) = release {
                message.push_str(&format!("; scan lease release failed: {release}"));
            } else if let Some(invalidation) = invalidation {
                message.push_str(&format!("; durable invalidation failed: {invalidation}"));
            }
            return Err(FacadeError::new(message));
        }
        Ok(PreparedQueryResult {
            result: QueryResult {
                cursor,
                projection,
                sequence: published.sequence,
            },
            lease,
            activation,
            snapshot_id: published.snapshot_id,
        })
    }

    pub fn finish_query_response(
        &mut self,
        prepared: PreparedQueryResult,
    ) -> Result<QueryResult, FacadeError> {
        self.service
            .store_mut()
            .release_query_lease(&prepared.lease, &prepared.activation)
            .map_err(|error| FacadeError::context("release scan response fence", error))?;
        Ok(prepared.result)
    }

    /// Releases a daemon-free response fence even if another direct caller
    /// has already activated a newer facade epoch for this root.
    pub fn finish_query_response_direct(
        &mut self,
        prepared: PreparedQueryResult,
    ) -> Result<QueryResult, FacadeError> {
        self.service
            .store_mut()
            .release_query_lease_direct(&prepared.lease)
            .map_err(|error| FacadeError::context("release direct scan response fence", error))?;
        Ok(prepared.result)
    }

    /// Renews the durable pins held by a prepared immutable scan response.
    ///
    /// Direct snapshot scans retain the prepared response beyond a bounded
    /// socket write, so they renew this fence until the caller commits or
    /// aborts its cursor.
    pub fn renew_query_response(
        &mut self,
        prepared: &PreparedQueryResult,
        now_ns: i64,
        ttl_ns: i64,
    ) -> Result<(), FacadeError> {
        let expires_ns = now_ns
            .checked_add(ttl_ns)
            .ok_or_else(|| FacadeError::new("scan lease renewal expiration overflow"))?;
        self.service
            .store_mut()
            .renew_query_lease(&prepared.lease, &prepared.activation, now_ns, expires_ns)
            .map_err(|error| FacadeError::context("renew scan response fence", error))
    }

    /// Renews a daemon-free response fence without depending on the latest
    /// facade epoch, which may belong to another overlapping process.
    pub fn renew_query_response_direct(
        &mut self,
        prepared: &PreparedQueryResult,
        now_ns: i64,
        ttl_ns: i64,
    ) -> Result<(), FacadeError> {
        let lease_expires_ns = now_ns
            .checked_add(ttl_ns)
            .ok_or_else(|| FacadeError::new("scan response lease expiration overflow"))?;
        self.service
            .store_mut()
            .renew_query_lease_direct(&prepared.lease, now_ns, lease_expires_ns)
            .map_err(|error| FacadeError::context("renew direct scan response fence", error))
    }

    /// Returns the still-pinned target snapshot path for a prepared response.
    pub fn prepared_snapshot_path(
        &self,
        prepared: &PreparedQueryResult,
    ) -> Result<PathBuf, FacadeError> {
        self.service
            .snapshot_path(prepared.snapshot_id)
            .map_err(|error| FacadeError::context("load prepared query snapshot path", error))
    }

    fn check_continuity_or_invalidate(
        &mut self,
        watch_id: [u8; 16],
        context: &str,
    ) -> Result<(), FacadeError> {
        let check = self
            .views
            .get(&watch_id)
            .ok_or_else(|| FacadeError::new("scan facade is not active"))?
            .monitor
            .check_continuity();
        if let Err(error) = check {
            let activation = self
                .views
                .get(&watch_id)
                .expect("failed check came from active view")
                .activation
                .clone();
            let invalidation = self
                .service
                .store_mut()
                .invalidate_snapshot_facade(&activation);
            return Err(match invalidation {
                Ok(()) => {
                    self.views.remove(&watch_id);
                    FacadeError::context(context, error)
                }
                Err(invalidation) => FacadeError::new(format!(
                    "{context}: {error}; durable invalidation failed: {invalidation}"
                )),
            });
        }
        Ok(())
    }

    fn snapshot_uuid(&self, snapshot_id: i64) -> Result<[u8; 16], FacadeError> {
        let bytes: Vec<u8> = self
            .service
            .store()
            .connection()
            .query_row(
                "SELECT subvol_uuid FROM snapshots WHERE id = ?1",
                [snapshot_id],
                |row| row.get(0),
            )
            .map_err(|error| FacadeError::context("load scan snapshot UUID", error))?;
        bytes
            .try_into()
            .map_err(|_| FacadeError::new("scan snapshot UUID has invalid length"))
    }

    fn snapshot_id_for_uuid(&self, uuid: [u8; 16]) -> Result<Option<i64>, FacadeError> {
        self.service
            .store()
            .connection()
            .query_row(
                "SELECT id FROM snapshots WHERE subvol_uuid = ?1 AND physical_state = 'present'",
                [uuid.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| FacadeError::context("load baseline snapshot id", error))
    }
}

#[derive(Debug)]
pub struct FacadeError {
    message: String,
}

impl FacadeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(context: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
    }
}

impl fmt::Display for FacadeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FacadeError {}
