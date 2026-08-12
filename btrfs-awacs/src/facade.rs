//! Synchronized immutable-scan facade for direct AWACS clients.

use crate::compat::{
    decode_direct_scan_cursor, encode_direct_scan_cursor, project_events, BoundaryKind,
    CursorClaims, Projection,
};
use crate::manager::{FacadeActivation, QueryLeaseReservation};
use crate::namespace::NamespaceMonitor;
use crate::namespace::ViewBinding;
use crate::precision::PrecisionGuard;
use crate::service::{ChangesOptions, Service};
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStringExt;
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
    precision: Option<PrecisionGuard>,
}

pub struct FacadeService {
    service: Service,
    views: BTreeMap<[u8; 16], ActiveView>,
    query_lease_owner: [u8; 16],
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

    pub fn view_binding(&self, watch_id: [u8; 16]) -> Option<&ViewBinding> {
        self.views.get(&watch_id).map(|view| view.monitor.binding())
    }

    pub fn has_precision_guard(&self, watch_id: [u8; 16]) -> bool {
        self.views
            .get(&watch_id)
            .is_some_and(|view| view.precision.is_some())
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
        // Every published revision is already proved fscrypt-free: full-index
        // publication rejects it, and incremental publication rejects target
        // objects before advancing the revision.  A descendant watch inherits
        // that accepted revision, so rehydrating the complete index here only
        // to repeat the fscrypt scan is both redundant and O(all paths).
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
                precision: None,
            },
        );
        tracing::info!(
            elapsed_ms = activation_started.elapsed().as_millis() as u64,
            root = %root.display(),
            "facade activation completed"
        );
        Ok(())
    }

    /// Enables the optional recursive precision journal. Failure does not
    /// invalidate direct scans; the durable guard epoch is marked gapped and
    /// future scans retain conservative coarse projection.
    pub fn activate_precision_guard(
        &mut self,
        watch_id: [u8; 16],
        marker_directory: &Path,
        now_ns: i64,
    ) -> Result<(), FacadeError> {
        self.check_continuity_or_invalidate(watch_id, "pre-arm precision namespace check")?;
        let activation = self
            .views
            .get(&watch_id)
            .ok_or_else(|| FacadeError::new("scan facade is not active"))?
            .activation
            .clone();
        let root = self
            .views
            .get(&watch_id)
            .expect("checked active view")
            .monitor
            .binding()
            .root_path
            .clone();
        let initial = self
            .service
            .store_mut()
            .begin_precision_guard(&activation)
            .map_err(|error| FacadeError::context("begin precision guard", error))?;
        let mut guard = match PrecisionGuard::arm(
            Path::new(&OsString::from_vec(root)),
            marker_directory,
            initial.epoch,
        ) {
            Ok(guard) => guard,
            Err(error) => {
                self.service
                    .store_mut()
                    .gap_precision_guard(&activation, initial.epoch)
                    .map_err(|gap| {
                        FacadeError::new(format!(
                            "arm precision guard: {error}; persist gap: {gap}"
                        ))
                    })?;
                return Err(FacadeError::context("arm precision guard", error));
            }
        };
        let cursor = guard
            .certify(self.service.store_mut(), &activation, now_ns)
            .map_err(|error| FacadeError::context("certify precision guard", error))?;
        self.service
            .store_mut()
            .complete_precision_guard(&activation, cursor)
            .map_err(|error| FacadeError::context("complete precision guard", error))?;
        self.views
            .get_mut(&watch_id)
            .expect("checked active view")
            .precision = Some(guard);
        Ok(())
    }

    /// Builds a direct-scan response whose durable lease pins only its target
    /// snapshot.
    ///
    /// The replay comparison still uses the exact retained prior boundary,
    /// but `historical_changes()` owns the short-lived pins needed while that
    /// comparison runs. The longer-lived scan lease must not retain the whole
    /// interval between the prior and selected snapshots.
    pub fn prepare_scan_query(
        &mut self,
        watch_id: [u8; 16],
        previous_baseline: Option<&crate::scan::SnapshotBaseline>,
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
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
        let metadata = self
            .service
            .store()
            .metadata()
            .map_err(|error| FacadeError::context("load direct cursor metadata", error))?;
        let previous_claims = previous_baseline.and_then(|baseline| {
            decode_direct_scan_cursor(&baseline.continuity_token, &metadata.clock_hmac_key)
                .ok()
                .filter(|claims| {
                    baseline.identity.read_only
                        && claims.target_snapshot_uuid == baseline.identity.subvolume_uuid
                })
        });
        self.prepare_scan_after_cut(
            activation,
            authorization_id,
            published,
            previous_claims.as_ref(),
            requester_uid,
            now_ns,
        )
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

    /// Returns the opaque continuity token prepared for the direct scan API.
    pub fn direct_scan_continuity_token(
        &self,
        prepared: &PreparedQueryResult,
    ) -> Result<Vec<u8>, FacadeError> {
        Ok(prepared.result.cursor.clone())
    }

    fn prepare_scan_after_cut(
        &mut self,
        activation: FacadeActivation,
        authorization_id: [u8; 16],
        published: crate::manager::PublishedCut,
        previous_claims: Option<&CursorClaims>,
        requester_uid: u32,
        now_ns: i64,
    ) -> Result<PreparedQueryResult, FacadeError> {
        let watch_id = activation.watch_id;
        let guard_cursor = {
            let (service, views) = (&mut self.service, &mut self.views);
            views
                .get_mut(&watch_id)
                .and_then(|view| view.precision.as_mut())
                .and_then(|guard| guard.certify(service.store_mut(), &activation, now_ns).ok())
        };
        self.check_continuity_or_invalidate(watch_id, "post-cut namespace check")?;
        let view = self.views.get(&watch_id).expect("checked active view");
        self.service
            .store_mut()
            .finalize_cut_boundary(
                &activation,
                view.monitor.binding(),
                published.sequence,
                guard_cursor,
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
        let from_boundary = previous_claims
            .and_then(|old| self.replay_boundary_for_cursor(old, &claims).ok().flatten());
        let lease_expires_ns = now_ns
            .checked_add(60_000_000_000)
            .ok_or_else(|| FacadeError::new("scan lease expiration overflow"))?;
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
        let projection = if let Some((_, from_snapshot_uuid)) = from_boundary {
            match self.service.historical_changes(
                watch_id,
                authorization_id,
                requester_uid,
                from_snapshot_uuid,
                snapshot_uuid,
                now_ns,
            ) {
                Ok(changes) if !changes.fresh_instance => project_events(&changes.events),
                Ok(_) => Projection {
                    fresh_instance: true,
                    paths: vec![b"/".to_vec()],
                },
                // Direct scans remain snapshot-correct without an
                // incremental proof: retain the selected target lease and
                // ask the client to scan all sparse paths from that immutable
                // root.
                Err(_) => Projection {
                    fresh_instance: true,
                    paths: vec![b"/".to_vec()],
                },
            }
        } else {
            Projection {
                fresh_instance: true,
                paths: vec![b"/".to_vec()],
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

    fn replay_boundary_for_cursor(
        &self,
        old: &CursorClaims,
        target: &CursorClaims,
    ) -> Result<Option<(i64, [u8; 16])>, FacadeError> {
        if old.format_version != target.format_version
            || old.store_uuid != target.store_uuid
            || old.watch_id != target.watch_id
            || old.cursor_epoch != target.cursor_epoch
            || old.owner_grant_id != target.owner_grant_id
            || old.monitor_session_id != target.monitor_session_id
            || old.boundary_kind != target.boundary_kind
            || old.algorithm_version != target.algorithm_version
            || old.cut_sequence > target.cut_sequence
        {
            return Ok(None);
        }
        let old_sequence = i64::try_from(old.cut_sequence)
            .map_err(|_| FacadeError::new("old cursor sequence overflow"))?;
        let retained: Option<(i64, Vec<u8>)> = self
            .service
            .store()
            .connection()
            .query_row(
                r#"SELECT b.cut_sequence, s.subvol_uuid
                     FROM fsmonitor_boundaries b
                     JOIN snapshots s ON s.id = b.target_snapshot_id
                    WHERE b.watch_id = ?1 AND b.cut_sequence = ?2
                      AND b.clock_epoch = ?3 AND s.physical_state = 'present'
                    LIMIT 1"#,
                rusqlite::params![
                    old.watch_id.as_slice(),
                    old_sequence,
                    old.cursor_epoch.as_slice(),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| FacadeError::context("resolve retained replay boundary", error))?;
        retained
            .map(|(sequence, uuid)| {
                let uuid = uuid
                    .try_into()
                    .map_err(|_| FacadeError::new("retained boundary UUID has invalid length"))?;
                Ok((sequence, uuid))
            })
            .transpose()
            .map(|retained| {
                retained.filter(|(sequence, uuid)| {
                    *sequence == old_sequence && *uuid == old.target_snapshot_uuid
                })
            })
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
