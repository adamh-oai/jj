//! Synchronized snapshot/query facade shared by Watchman and Git transports.

use crate::compat::{
    decode_clock, encode_clock, project_events, BoundaryKind, ClockClaims, Projection,
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
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const ALGORITHM_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResult {
    pub clock: String,
    pub projection: Projection,
    pub sequence: i64,
}

/// A projected query whose immutable inputs remain pinned until the transport
/// finishes its bounded response write.  The fields which authorize release
/// stay private so a caller cannot manufacture or retarget a response fence.
pub struct PreparedQueryResult {
    pub result: QueryResult,
    lease: QueryLeaseReservation,
    activation: FacadeActivation,
}

pub struct PendingQueryCut {
    worker: Service,
    activation: FacadeActivation,
    authorization_id: [u8; 16],
    watch_id: [u8; 16],
    old_clock: Option<String>,
    requester_uid: u32,
    requester_gid: u32,
    now_ns: i64,
}

pub struct CompletedQueryCut {
    activation: FacadeActivation,
    authorization_id: [u8; 16],
    old_clock: Option<String>,
    requester_uid: u32,
    now_ns: i64,
    published: crate::manager::PublishedCut,
}

impl PendingQueryCut {
    pub fn execute(mut self) -> Result<CompletedQueryCut, FacadeError> {
        let published = self
            .worker
            .changes(&ChangesOptions {
                watch_id: self.watch_id,
                authorization_id: self.authorization_id,
                requester_uid: self.requester_uid,
                requester_gid: self.requester_gid,
                now_ns: self.now_ns,
            })
            .map_err(|error| FacadeError::context("take concurrent query cut", error))?;
        Ok(CompletedQueryCut {
            activation: self.activation,
            authorization_id: self.authorization_id,
            old_clock: self.old_clock,
            requester_uid: self.requester_uid,
            now_ns: self.now_ns,
            published,
        })
    }
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

    pub fn precision_readiness_fds(
        &self,
        watch_ids: &[[u8; 16]],
    ) -> Result<Vec<OwnedFd>, FacadeError> {
        watch_ids
            .iter()
            .filter_map(|watch_id| {
                self.views
                    .get(watch_id)
                    .and_then(|view| view.precision.as_ref())
            })
            .map(|guard| {
                guard.duplicate_readiness_fd().map_err(|error| {
                    FacadeError::context("duplicate precision readiness descriptor", error)
                })
            })
            .collect()
    }

    pub fn verified_view_root(&mut self, watch_id: [u8; 16]) -> Result<PathBuf, FacadeError> {
        self.check_continuity_or_invalidate(watch_id, "trigger namespace check")?;
        let root = self
            .views
            .get(&watch_id)
            .expect("checked active view")
            .monitor
            .binding()
            .root_path
            .clone();
        Ok(PathBuf::from(OsString::from_vec(root)))
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
    /// invalidate the snapshot facade; the durable guard epoch is marked
    /// gapped and future queries retain conservative coarse projection.
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
            .ok_or_else(|| FacadeError::new("watch facade is not active"))?
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

    #[cfg(any())]
    fn legacy_activate_proved_worktree(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        root: &Path,
    ) -> Result<String, FacadeError> {
        if !self
            .service
            .ensure_snapshot_facade_is_enabled(watch_id)
            .map_err(|error| FacadeError::context("verify dirty-witness ABI", error))?
        {
            return Err(FacadeError::new(
                "snapshot facade is disabled until the experimental dirty-witness ABI is verified",
            ));
        }
        let handoff = self
            .service
            .take_worktree_view_handoff(watch_id, authorization_id, root)
            .ok_or_else(|| {
                FacadeError::new(
                    "proved Worktree seed requires its live pre-publication monitor handoff",
                )
            })?;
        if let Err(error) = handoff.monitor.check_continuity() {
            let _ = self
                .service
                .store_mut()
                .invalidate_snapshot_facade(&handoff.activation);
            return Err(FacadeError::context(
                "validate Worktree monitor handoff",
                error,
            ));
        }
        let activation = handoff.activation.clone();
        let snapshot_uuid = handoff.snapshot_uuid;
        self.views.insert(
            watch_id,
            ActiveView {
                authorization_id,
                activation: handoff.activation,
                monitor: handoff.monitor,
                precision: None,
            },
        );
        self.check_continuity_or_invalidate(watch_id, "final Worktree seed view check")?;
        let metadata = self
            .service
            .store()
            .metadata()
            .map_err(|error| FacadeError::context("load seed clock metadata", error))?;
        Ok(encode_clock(
            &ClockClaims {
                format_version: metadata.clock_format_version,
                store_uuid: metadata.store_uuid,
                watch_id,
                clock_epoch: activation.clock_epoch,
                cut_sequence: 0,
                owner_grant_id: authorization_id,
                monitor_session_id: activation.monitor_session_id,
                boundary_kind: BoundaryKind::ProvedWorktreeSeed,
                algorithm_version: ALGORITHM_VERSION,
                target_snapshot_uuid: snapshot_uuid,
            },
            &metadata.clock_hmac_key,
        ))
    }

    #[cfg(any())]
    fn legacy_has_proved_worktree_handoff(&self, watch_id: [u8; 16]) -> bool {
        self.service.has_worktree_view_handoff(watch_id)
    }

    pub fn query(
        &mut self,
        watch_id: [u8; 16],
        old_clock: Option<&str>,
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
    ) -> Result<QueryResult, FacadeError> {
        let prepared =
            self.prepare_query(watch_id, old_clock, requester_uid, requester_gid, now_ns)?;
        self.finish_query_response(prepared)
    }

    /// Builds a response while retaining its query lease and history pins.
    /// A transport must call `finish_query_response` after its bounded write,
    /// including when that write fails.
    pub fn prepare_query(
        &mut self,
        watch_id: [u8; 16],
        old_clock: Option<&str>,
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
            .map_err(|error| FacadeError::context("take synchronized query cut", error))?;
        self.prepare_query_after_cut(
            activation,
            authorization_id,
            published,
            old_clock,
            requester_uid,
            now_ns,
        )
    }

    pub fn begin_concurrent_query(
        &mut self,
        watch_id: [u8; 16],
        old_clock: Option<&str>,
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
    ) -> Result<PendingQueryCut, FacadeError> {
        self.check_continuity_or_invalidate(watch_id, "pre-cut namespace check")?;
        let view = self.views.get(&watch_id).expect("checked active view");
        let authorization_id = view.authorization_id;
        let activation = view.activation.clone();
        let worker = self
            .service
            .query_worker()
            .map_err(|error| FacadeError::context("create concurrent query worker", error))?;
        Ok(PendingQueryCut {
            worker,
            activation,
            authorization_id,
            watch_id,
            old_clock: old_clock.map(str::to_owned),
            requester_uid,
            requester_gid,
            now_ns,
        })
    }

    pub fn finish_concurrent_query(
        &mut self,
        completed: CompletedQueryCut,
    ) -> Result<PreparedQueryResult, FacadeError> {
        self.prepare_query_after_cut(
            completed.activation,
            completed.authorization_id,
            completed.published,
            completed.old_clock.as_deref(),
            completed.requester_uid,
            completed.now_ns,
        )
    }

    fn prepare_query_after_cut(
        &mut self,
        activation: FacadeActivation,
        authorization_id: [u8; 16],
        published: crate::manager::PublishedCut,
        old_clock: Option<&str>,
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
            .map_err(|error| FacadeError::context("finalize query boundary", error))?;
        self.check_continuity_or_invalidate(watch_id, "pre-response namespace check")?;

        let metadata = self
            .service
            .store()
            .metadata()
            .map_err(|error| FacadeError::context("load clock metadata", error))?;
        let snapshot_uuid = self.snapshot_uuid(published.snapshot_id)?;
        let claims = ClockClaims {
            format_version: metadata.clock_format_version,
            store_uuid: metadata.store_uuid,
            watch_id,
            clock_epoch: activation.clock_epoch,
            cut_sequence: u64::try_from(published.sequence)
                .map_err(|_| FacadeError::new("negative cut sequence"))?,
            owner_grant_id: authorization_id,
            monitor_session_id: activation.monitor_session_id,
            boundary_kind: BoundaryKind::Cut,
            algorithm_version: ALGORITHM_VERSION,
            target_snapshot_uuid: snapshot_uuid,
        };
        let from_boundary = old_clock.and_then(|token| {
            decode_clock(token, &metadata.clock_hmac_key)
                .ok()
                .and_then(|old| self.replay_boundary_for_clock(&old, &claims).ok().flatten())
        });
        let from_sequence = from_boundary.map(|(sequence, _)| sequence);
        let lease_expires_ns = now_ns
            .checked_add(60_000_000_000)
            .ok_or_else(|| FacadeError::new("query lease expiration overflow"))?;
        let lease = self
            .service
            .store_mut()
            .begin_query_lease(
                &activation,
                from_sequence,
                published.sequence,
                self.query_lease_owner,
                lease_expires_ns,
            )
            .map_err(|error| FacadeError::context("pin query inputs", error))?;
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
                Err(error) => {
                    let release = self
                        .service
                        .store_mut()
                        .release_query_lease(&lease, &activation);
                    return Err(match release {
                        Ok(()) => FacadeError::context("compare retained snapshots", error),
                        Err(release) => FacadeError::new(format!(
                            "compare retained snapshots: {error}; release failed: {release}"
                        )),
                    });
                }
            }
        } else {
            Projection {
                fresh_instance: true,
                paths: vec![b"/".to_vec()],
            }
        };
        let clock = encode_clock(&claims, &metadata.clock_hmac_key);
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
                message.push_str(&format!("; query lease release failed: {release}"));
            } else if let Some(invalidation) = invalidation {
                message.push_str(&format!("; durable invalidation failed: {invalidation}"));
            }
            return Err(FacadeError::new(message));
        }
        Ok(PreparedQueryResult {
            result: QueryResult {
                clock,
                projection,
                sequence: published.sequence,
            },
            lease,
            activation,
        })
    }

    pub fn finish_query_response(
        &mut self,
        prepared: PreparedQueryResult,
    ) -> Result<QueryResult, FacadeError> {
        self.service
            .store_mut()
            .release_query_lease(&prepared.lease, &prepared.activation)
            .map_err(|error| FacadeError::context("release query response fence", error))?;
        Ok(prepared.result)
    }

    fn check_continuity_or_invalidate(
        &mut self,
        watch_id: [u8; 16],
        context: &str,
    ) -> Result<(), FacadeError> {
        let check = self
            .views
            .get(&watch_id)
            .ok_or_else(|| FacadeError::new("watch facade is not active"))?
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

    fn replay_boundary_for_clock(
        &self,
        old: &ClockClaims,
        target: &ClockClaims,
    ) -> Result<Option<(i64, [u8; 16])>, FacadeError> {
        if old.format_version != target.format_version
            || old.store_uuid != target.store_uuid
            || old.watch_id != target.watch_id
            || old.clock_epoch != target.clock_epoch
            || old.owner_grant_id != target.owner_grant_id
            || old.monitor_session_id != target.monitor_session_id
            || old.algorithm_version != target.algorithm_version
            || old.cut_sequence > target.cut_sequence
        {
            return Ok(None);
        }
        let old_sequence = i64::try_from(old.cut_sequence)
            .map_err(|_| FacadeError::new("old clock sequence overflow"))?;
        // The exact old snapshot may have been compacted. A retained older
        // boundary is still safe: one direct retained-to-head comparison can
        // only over-report paths, and the response advances the client.
        let retained: Option<(i64, Vec<u8>)> = self
            .service
            .store()
            .connection()
            .query_row(
                r#"SELECT b.cut_sequence, s.subvol_uuid
                     FROM fsmonitor_boundaries b
                     JOIN snapshots s ON s.id = b.target_snapshot_id
                    WHERE b.watch_id = ?1 AND b.cut_sequence <= ?2
                      AND b.clock_epoch = ?3 AND s.physical_state = 'present'
                    ORDER BY b.cut_sequence DESC LIMIT 1"#,
                rusqlite::params![
                    old.watch_id.as_slice(),
                    old_sequence,
                    old.clock_epoch.as_slice(),
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
            .map_err(|error| FacadeError::context("load query snapshot UUID", error))?;
        bytes
            .try_into()
            .map_err(|_| FacadeError::new("query snapshot UUID has invalid length"))
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
