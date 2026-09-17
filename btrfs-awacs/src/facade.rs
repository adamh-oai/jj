//! Synchronized immutable-scan facade for direct AWACS clients.

use crate::compat::{
    BoundaryKind, CursorClaims, Projection, decode_direct_scan_cursor, encode_direct_scan_cursor,
    project_events,
};
use crate::manager::{FacadeActivation, QueryLeaseReservation};
use crate::namespace::NamespaceMonitor;
use crate::namespace::ViewBinding;
use crate::service::{ChangesOptions, Service};
use rusqlite::{OptionalExtension, params};
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
    allow_replay_invalidation: bool,
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

    /// Returns the immutable baseline explicitly retained for the watch-local
    /// Git consumer.
    ///
    /// Fresh worktree creation explicitly pins snapshot A for its Git lane
    /// before materializing B. That pin, rather than the oldest recorded cut,
    /// proves which baseline the creation path may advance to a verified
    /// cursor. Existing indexes without a valid Git token cannot establish
    /// provenance merely because some snapshot remains in the watch history.
    pub fn initial_scan_baseline(
        &self,
        watch_id: [u8; 16],
    ) -> Result<crate::scan::SnapshotBaseline, FacadeError> {
        let (filesystem_uuid, subvolume_uuid, read_only) =
            retained_git_baseline_identity(self.service.store().connection(), watch_id)
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
            // The first consumer has no prior cursor to verify. The
            // immutable identity above is sufficient to select sequence zero;
            // the response supplies the first integrity-checked continuity token.
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
        allow_replay_invalidation: bool,
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
        // The opaque cursor identifies this exact immutable snapshot
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
            allow_replay_invalidation,
        })
    }

    /// Validates a Git cursor against its retained logical journal boundary.
    ///
    /// Git may publish hook responses out of issue order. Its historical
    /// cursor therefore owns no exclusive physical snapshot pin: the durable
    /// boundary, authenticated claims, and adjacent event spool prove replay.
    /// Internal index seeding uses validation alone while preparing a cursor.
    /// Incoming Git queries and verified index publication separately call
    /// observe_logical_git_baseline() once there is evidence of persistence.
    pub fn validate_logical_git_baseline(
        &self,
        watch_id: [u8; 16],
        baseline: &crate::scan::SnapshotBaseline,
    ) -> Result<bool, FacadeError> {
        let claims = self.logical_git_baseline_claims(watch_id, baseline)?;
        let sequence = i64::try_from(claims.cut_sequence)
            .map_err(|_| FacadeError::new("Git logical cursor sequence exceeds SQLite range"))?;
        let (filesystem_uuid, indexed_sequence, replay_floor): (Vec<u8>, i64, i64) = self
            .service
            .store()
            .connection()
            .query_row(
                r#"SELECT f.fs_uuid, w.indexed_seq, w.replay_floor_seq
                     FROM watches w JOIN filesystems f ON f.id = w.filesystem_id
                    WHERE w.id = ?1 AND w.state = 'active'"#,
                [watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| FacadeError::context("load Git logical cursor watch", error))?;
        if filesystem_uuid != baseline.identity.filesystem_uuid {
            return Err(FacadeError::new(
                "Git logical cursor filesystem does not match its watch",
            ));
        }
        if sequence > indexed_sequence {
            return Err(FacadeError::new(
                "Git logical cursor is newer than the indexed watch head",
            ));
        }
        if sequence < replay_floor {
            return Ok(false);
        }
        let boundary: Option<(Vec<u8>, Vec<u8>)> = self
            .service
            .store()
            .connection()
            .query_row(
                r#"SELECT b.clock_epoch, s.subvol_uuid
                     FROM fsmonitor_boundaries b
                     JOIN snapshots s ON s.id = b.target_snapshot_id
                    WHERE b.watch_id = ?1 AND b.cut_sequence = ?2"#,
                params![watch_id.as_slice(), sequence],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| FacadeError::context("load Git logical cursor boundary", error))?;
        let Some((boundary_epoch, snapshot_uuid)) = boundary else {
            return Ok(false);
        };
        if boundary_epoch != claims.cursor_epoch || snapshot_uuid != claims.target_snapshot_uuid {
            return Err(FacadeError::new(
                "Git logical cursor does not match its recorded journal boundary",
            ));
        }
        Ok(true)
    }

    /// Records a valid incoming Git query or a cursor verified by index
    /// readback. Merely issuing a cursor or completing its response is not
    /// evidence that Git persisted it and must not call this method.
    /// Boundary validation and observation share a writer transaction with
    /// retention so GC cannot remove history between the check and the pin.
    /// A pruned cursor still requests Git's whole-index invalidation fallback.
    pub fn observe_logical_git_baseline(
        &mut self,
        watch_id: [u8; 16],
        baseline: &crate::scan::SnapshotBaseline,
        now_ns: i64,
    ) -> Result<bool, FacadeError> {
        let claims = self.logical_git_baseline_claims(watch_id, baseline)?;
        let observation = crate::manager::GitCursorObservation {
            watch_id,
            authorization_id: claims.owner_grant_id,
            filesystem_uuid: baseline.identity.filesystem_uuid,
            snapshot_uuid: claims.target_snapshot_uuid,
            clock_epoch: claims.cursor_epoch,
            sequence: i64::try_from(claims.cut_sequence).map_err(|_| {
                FacadeError::new("Git logical cursor sequence exceeds SQLite range")
            })?,
            now_ns,
            grace_ns: self.service.git_cursor_grace_ns(),
        };
        self.service
            .store_mut()
            .record_git_cursor_observation(&observation)
            .map_err(|error| FacadeError::context("observe Git query cursor", error))
    }

    fn logical_git_baseline_claims(
        &self,
        watch_id: [u8; 16],
        baseline: &crate::scan::SnapshotBaseline,
    ) -> Result<CursorClaims, FacadeError> {
        let view = self
            .views
            .get(&watch_id)
            .ok_or_else(|| FacadeError::new("scan facade is not active"))?;
        if !baseline.identity.read_only || baseline.retention_token != watch_id {
            return Err(FacadeError::new(
                "Git logical cursor does not identify this watch-owned immutable lane",
            ));
        }
        let claims = decode_direct_scan_cursor(&baseline.continuity_token).map_err(|error| {
            FacadeError::context("validate Git logical cursor integrity", error)
        })?;
        let metadata = self
            .service
            .store()
            .metadata()
            .map_err(|error| FacadeError::context("load Git logical cursor metadata", error))?;
        if claims.format_version != metadata.clock_format_version
            || claims.store_uuid != metadata.store_uuid
            || claims.watch_id != watch_id
            || claims.owner_grant_id != view.authorization_id
            || claims.target_snapshot_uuid != baseline.identity.subvolume_uuid
            || claims.algorithm_version != ALGORITHM_VERSION
        {
            return Err(FacadeError::new(
                "Git logical cursor manager, watch, authorization, or snapshot claims are stale",
            ));
        }
        if claims.cursor_epoch == view.activation.clock_epoch
            && claims.monitor_session_id != view.activation.monitor_session_id
        {
            return Err(FacadeError::new(
                "Git logical cursor monitor session does not match its active epoch",
            ));
        }
        Ok(claims)
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
            allow_replay_invalidation,
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
                Ok(changes) if allow_replay_invalidation => {
                    tracing::info!(
                        from_sequence = changes.from_sequence,
                        to_sequence = changes.to_sequence,
                        "Git logical cursor exceeds retained exact replay; invalidating the index"
                    );
                    Projection {
                        fresh_instance: true,
                        paths: vec![b"/".to_vec()],
                        prefixes: Vec::new(),
                    }
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
        let cursor = encode_direct_scan_cursor(&claims);
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

fn retained_git_baseline_identity(
    connection: &rusqlite::Connection,
    watch_id: [u8; 16],
) -> rusqlite::Result<(Vec<u8>, Vec<u8>, i64)> {
    connection.query_row(
        r#"SELECT f.fs_uuid, s.subvol_uuid, s.readonly
             FROM watches w
             JOIN snapshot_pins p ON p.owner_kind = 'consumer-baseline'
                                 AND p.owner_id = w.id
                                 AND p.reason = 'committed'
             JOIN snapshots s ON s.id = p.snapshot_id
                             AND s.physical_state = 'present'
             JOIN revisions r ON r.snapshot_id = s.id
                             AND r.state = 'ready'
             JOIN filesystems f ON f.id = s.filesystem_id
            WHERE w.id = ?1 AND w.state = 'active'"#,
        [watch_id.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::retained_git_baseline_identity;

    #[test]
    fn initial_git_baseline_requires_its_own_present_ready_pin() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE watches (id BLOB, state TEXT);
                CREATE TABLE snapshot_pins (
                    snapshot_id INTEGER,
                    owner_kind TEXT,
                    owner_id BLOB,
                    reason TEXT
                );
                CREATE TABLE snapshots (
                    id INTEGER,
                    filesystem_id INTEGER,
                    subvol_uuid BLOB,
                    readonly INTEGER,
                    physical_state TEXT
                );
                CREATE TABLE revisions (snapshot_id INTEGER, state TEXT);
                CREATE TABLE filesystems (id INTEGER, fs_uuid BLOB);
                "#,
            )
            .unwrap();
        let watch_id = [1; 16];
        let jj_owner_id = [2; 16];
        let filesystem_uuid = [3; 16];
        let deleted_snapshot_uuid = [4; 16];
        let present_snapshot_uuid = [5; 16];
        connection
            .execute(
                "INSERT INTO watches(id, state) VALUES (?1, 'active')",
                [watch_id.as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO filesystems(id, fs_uuid) VALUES (1, ?1)",
                [filesystem_uuid.as_slice()],
            )
            .unwrap();
        for (snapshot_id, snapshot_uuid, state) in [
            (48, deleted_snapshot_uuid, "deleted"),
            (49, present_snapshot_uuid, "present"),
        ] {
            connection
                .execute(
                    "INSERT INTO snapshots(id, filesystem_id, subvol_uuid, readonly, physical_state) VALUES (?1, 1, ?2, 1, ?3)",
                    params![snapshot_id, snapshot_uuid.as_slice(), state],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO revisions(snapshot_id, state) VALUES (?1, 'ready')",
                    [snapshot_id],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) VALUES (49, 'consumer-baseline', ?1, 'committed')",
                [jj_owner_id.as_slice()],
            )
            .unwrap();

        assert!(matches!(
            retained_git_baseline_identity(&connection, watch_id),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));

        connection
            .execute(
                "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) VALUES (48, 'consumer-baseline', ?1, 'committed')",
                [watch_id.as_slice()],
            )
            .unwrap();
        assert!(matches!(
            retained_git_baseline_identity(&connection, watch_id),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));

        connection
            .execute(
                "UPDATE snapshot_pins SET snapshot_id = 49 WHERE owner_id = ?1",
                [watch_id.as_slice()],
            )
            .unwrap();
        assert_eq!(
            retained_git_baseline_identity(&connection, watch_id).unwrap(),
            (filesystem_uuid.to_vec(), present_snapshot_uuid.to_vec(), 1)
        );
    }
}
