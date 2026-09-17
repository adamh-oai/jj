//! Direct scan sessions backed by the synchronized snapshot facade.

use crate::bootstrap::{RootInitPaths, git_admin_directory, open_initialized_root_service};
use crate::btrfs::{filesystem_info, subvolume_info};
use crate::facade::{FacadeService, PreparedQueryResult};
use crate::manager::{PERMISSION_CUT, PERMISSION_READ};
use crate::scan::{
    BeginScanRequest, Invalidation, ScanClient, ScanError, ScanErrorKind, ScanOutcome, ScanSession,
    SnapshotBaseline, SnapshotIdentity, SnapshotLease,
};
use crate::status::{GitFsmonitorStatus, IndexChecksum, inspect_git_index};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
#[cfg(debug_assertions)]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SCAN_TTL_NS: i64 = 300_000_000_000;
#[cfg(debug_assertions)]
const TEST_SHORT_SCAN_TTL_NS: i64 = 300_000_000;

/// Direct in-process scan client.
///
/// The current facade/store implementation still owns the richer historical
/// manager schema, but callers do not need a persistent user process or scan
/// socket. Every operation opens the root's initialized state in this
/// process and serializes publication through a per-root filesystem lock.
pub struct DirectScanClient {
    handler: Arc<Mutex<DirectScanHandler>>,
    root_lock: Arc<File>,
}

impl DirectScanClient {
    /// Opens an initialized root and binds a direct handler to its local
    /// manager state. This is the production path used by embedded JJ.
    pub fn for_root(live_root: &Path) -> Result<Self, ScanError> {
        let open_started = Instant::now();
        let root = std::fs::canonicalize(live_root)
            .map_err(|error| unavailable(format!("canonicalize AWACS scan root: {error}")))?;
        let paths = RootInitPaths::from_environment(&root)
            .map_err(|error| unavailable(format!("resolve AWACS root state: {error}")))?;
        let state_dir = paths
            .manager_db
            .parent()
            .ok_or_else(|| other("AWACS manager database has no state directory"))?;
        let root_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(state_dir.join("root.lock"))
            .map_err(|error| unavailable(format!("open AWACS root lock: {error}")))?;
        let service = {
            let lock_started = Instant::now();
            let _state_lock = RootLockGuard::acquire(&root_lock)?;
            tracing::info!(
                elapsed_ms = lock_started.elapsed().as_millis() as u64,
                root = %root.display(),
                "direct scan root-state lock acquired for open"
            );
            let service_open_started = Instant::now();
            let service = open_initialized_root_service(&root)
                .map_err(|error| unavailable(format!("open initialized AWACS root: {error}")))?;
            tracing::info!(
                elapsed_ms = service_open_started.elapsed().as_millis() as u64,
                root = %root.display(),
                "direct scan root state opened"
            );
            service
        };
        let facade = Arc::new(Mutex::new(FacadeService::new(service)));
        tracing::info!(
            elapsed_ms = open_started.elapsed().as_millis() as u64,
            root = %root.display(),
            "direct scan client opened"
        );
        Ok(Self {
            handler: Arc::new(Mutex::new(DirectScanHandler::new(
                facade,
                unsafe { libc::geteuid() },
                unsafe { libc::getegid() },
            ))),
            root_lock: Arc::new(root_lock),
        })
    }

    fn lock_root(&self) -> Result<RootLockGuard<'_>, ScanError> {
        RootLockGuard::acquire(&self.root_lock)
    }

    /// Returns the exact sequence-zero baseline for a first consumer which
    /// does not have an AWACS token yet.
    pub fn initial_baseline(&mut self, live_root: &Path) -> Result<SnapshotBaseline, ScanError> {
        let _root_lock = self.lock_root()?;
        self.handler
            .lock()
            .map_err(|_| other("AWACS direct scan handler lock poisoned"))?
            .initial_baseline(live_root)
    }

    /// Computes the exact bridge between two leased immutable cuts, including
    /// cuts owned by different watches. Both leases are renewed before and
    /// after the comparison; the caller must keep them alive until the returned
    /// invalidations are applied to its inherited state.
    /// Nested subvolume contents are outside these root cuts; callers cloning
    /// those contents must separately invalidate their boundary prefixes.
    pub fn compare_scan_roots(
        &mut self,
        from: &mut SnapshotLease,
        to: &mut SnapshotLease,
    ) -> Result<Invalidation, ScanError> {
        // Renewal acquires each lease's root lock independently. Never do it
        // while holding this client's lock: either lease may share our root.
        from.renew()?;
        to.renew()?;
        crate::scan::validate_scan_root(from.scan_root(), from.next_baseline.identity)?;
        crate::scan::validate_scan_root(to.scan_root(), to.next_baseline.identity)?;
        let events = {
            let _root_lock = self.lock_root()?;
            let handler = self
                .handler
                .lock()
                .map_err(|_| other("AWACS direct scan handler lock poisoned"))?;
            let facade = handler
                .facade
                .lock()
                .map_err(|_| other("AWACS direct scan facade lock poisoned"))?;
            facade
                .service()
                .compare_snapshot_roots(from.scan_root().as_fd(), to.scan_root().as_fd())
                .map_err(|error| other(format!("compare AWACS scan roots: {error}")))?
        };
        from.renew()?;
        to.renew()?;
        crate::scan::validate_scan_root(from.scan_root(), from.next_baseline.identity)?;
        crate::scan::validate_scan_root(to.scan_root(), to.next_baseline.identity)?;
        direct_invalidation(&crate::compat::project_events(&events), false)
    }

    /// Retains a cursor verified in this worktree's published Git index.
    ///
    /// Call after publishing the index and before finishing its scan lease,
    /// which protects the cursor until the durable observation is recorded.
    /// Unlike issuing a hook response, checksum-valid index readback proves
    /// that Git can later present this cursor even if JJ advances first.
    pub fn observe_persisted_git_index(
        &mut self,
        live_root: &Path,
        expected_baseline: &SnapshotBaseline,
    ) -> Result<(), ScanError> {
        let _root_lock = self.lock_root()?;
        self.handler
            .lock()
            .map_err(|_| other("AWACS direct scan handler lock poisoned"))?
            .observe_persisted_git_index(live_root, expected_baseline)
    }
}

impl ScanClient for DirectScanClient {
    fn begin_scan(&mut self, request: &BeginScanRequest) -> Result<SnapshotLease, ScanError> {
        let begin_started = Instant::now();
        let root_lock_started = Instant::now();
        let _root_lock = self.lock_root()?;
        tracing::info!(
            elapsed_ms = root_lock_started.elapsed().as_millis() as u64,
            root = %request.live_root.display(),
            "direct scan root lock acquired for begin"
        );
        let handler_started = Instant::now();
        let response = self
            .handler
            .lock()
            .map_err(|_| other("AWACS direct scan handler lock poisoned"))?
            .begin_scan(request.clone())?;
        tracing::info!(
            elapsed_ms = handler_started.elapsed().as_millis() as u64,
            root = %request.live_root.display(),
            had_previous_baseline = request.previous_baseline.is_some(),
            "direct scan handler prepared begin response"
        );
        let PreparedScanLease {
            session_id,
            next_baseline,
            invalidation,
            expires_boottime_ns,
            scan_root,
        } = response;
        let lease = SnapshotLease::new(
            next_baseline,
            invalidation,
            expires_boottime_ns,
            scan_root,
            Box::new(DirectScanSession {
                handler: Arc::clone(&self.handler),
                root_lock: Arc::clone(&self.root_lock),
                session_id,
            }),
        );
        tracing::info!(
            elapsed_ms = begin_started.elapsed().as_millis() as u64,
            root = %request.live_root.display(),
            "direct scan begin completed"
        );
        Ok(lease)
    }

    fn release_baseline(&mut self, baseline_owner_id: [u8; 16]) -> Result<(), ScanError> {
        timed_scan_phase("release baseline client", || {
            let _root_lock = timed_scan_phase("release root lock", || self.lock_root())?;
            let mut handler = timed_scan_phase("release handler lock", || {
                self.handler
                    .lock()
                    .map_err(|_| other("AWACS direct scan handler lock poisoned"))
            })?;
            handler.release_baseline(baseline_owner_id)
        })
    }
}

struct DirectScanSession {
    handler: Arc<Mutex<DirectScanHandler>>,
    root_lock: Arc<File>,
    session_id: Vec<u8>,
}

impl DirectScanSession {
    fn lock_root(&self) -> Result<RootLockGuard<'_>, ScanError> {
        RootLockGuard::acquire(&self.root_lock)
    }
}

impl ScanSession for DirectScanSession {
    fn renew(&mut self) -> Result<(), ScanError> {
        let _root_lock = self.lock_root()?;
        self.handler
            .lock()
            .map_err(|_| other("AWACS direct scan handler lock poisoned"))?
            .renew_scan(&self.session_id)
    }

    fn promote(&mut self) -> Result<(), ScanError> {
        let _root_lock = self.lock_root()?;
        self.handler
            .lock()
            .map_err(|_| other("AWACS direct scan handler lock poisoned"))?
            .promote_scan(&self.session_id)
    }

    fn finish(&mut self, outcome: ScanOutcome) -> Result<(), ScanError> {
        timed_scan_phase("finish session", || {
            let _root_lock = timed_scan_phase("finish root lock", || self.lock_root())?;
            let mut handler = timed_scan_phase("finish handler lock", || {
                self.handler
                    .lock()
                    .map_err(|_| other("AWACS direct scan handler lock poisoned"))
            })?;
            handler.finish_scan(&self.session_id, outcome)
        })
    }
}

struct RootLockGuard<'a> {
    file: &'a File,
}

impl<'a> RootLockGuard<'a> {
    fn acquire(file: &'a File) -> Result<Self, ScanError> {
        // SAFETY: flock only inspects the valid open file descriptor and
        // blocks until another cooperating AWACS process releases it.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(other(format!(
                "lock AWACS root state: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self { file })
    }
}

impl Drop for RootLockGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: unlocking the same still-open descriptor is best-effort
        // cleanup; there is no memory-safety precondition.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// In-process direct-scan handler sharing one root-aware snapshot facade.
///
/// Direct continuity tokens wrap the existing authenticated cut claims under a
/// distinct scan domain. Exact replay is accepted only when the facade can
/// retain that same cut sequence and target snapshot UUID.
pub struct DirectScanHandler {
    facade: Arc<Mutex<FacadeService>>,
    requester_uid: u32,
    requester_gid: u32,
    sessions: HashMap<Vec<u8>, ActiveScanSession>,
    finished_sessions: HashMap<Vec<u8>, i64>,
    #[cfg(debug_assertions)]
    test_control_dir: Option<PathBuf>,
}

struct ActiveScanSession {
    prepared: PreparedQueryResult,
    baseline_owner_id: [u8; 16],
    logical_git_consumer: bool,
    expires_ns: i64,
}

struct PreparedScanLease {
    session_id: Vec<u8>,
    next_baseline: SnapshotBaseline,
    invalidation: Invalidation,
    expires_boottime_ns: u64,
    scan_root: File,
}

impl DirectScanHandler {
    /// Creates the embedded direct handler. Logical cursor replay shares one
    /// revision timeline, while each durable consumer keeps its own baseline
    /// snapshot pin until its local journal commits the next cursor.
    pub fn new(facade: Arc<Mutex<FacadeService>>, requester_uid: u32, requester_gid: u32) -> Self {
        Self {
            facade,
            requester_uid,
            requester_gid,
            sessions: HashMap::new(),
            finished_sessions: HashMap::new(),
            #[cfg(debug_assertions)]
            test_control_dir: std::env::var_os("BTRFS_AWACS_SCAN_TEST_CONTROL_DIR")
                .map(PathBuf::from),
        }
    }

    /// Canonicalizes, authorizes, and activates one requested root for this
    /// process. Durable watch state remains in the manager store; the
    /// in-memory facade view is rebuilt lazily when a root is first scanned.
    fn ensure_registered_root(
        &self,
        facade: &mut FacadeService,
        requested_root: &Path,
    ) -> Result<(PathBuf, [u8; 16]), ScanError> {
        let root = std::fs::canonicalize(requested_root)
            .map_err(|err| unavailable(format!("canonicalize AWACS scan root: {err}")))?;
        let root_bytes = root.as_os_str().as_bytes();
        let root_file =
            File::open(&root).map_err(|err| unavailable(format!("open AWACS scan root: {err}")))?;
        let filesystem = filesystem_info(root_file.as_fd())
            .map_err(|err| unavailable(format!("inspect AWACS scan filesystem: {err}")))?;
        let subvolume = subvolume_info(root_file.as_fd())
            .map_err(|err| unavailable(format!("inspect AWACS scan subvolume: {err}")))?;
        let mut existing = facade
            .service()
            .store()
            .active_uid_watch_at_path(
                root_bytes,
                self.requester_uid,
                PERMISSION_READ | PERMISSION_CUT,
            )
            .map_err(|err| other(format!("find existing AWACS scan root: {err}")))?;
        if existing.is_none() {
            existing = facade
                .service_mut()
                .store_mut()
                .rebind_active_uid_watch_path_by_subvolume(
                    filesystem.fs_uuid,
                    subvolume.uuid,
                    root_bytes,
                    self.requester_uid,
                    PERMISSION_READ | PERMISSION_CUT,
                )
                .map_err(|err| other(format!("rebind moved AWACS scan root: {err}")))?;
            if let Some((watch_id, _)) = existing {
                facade.forget_view(watch_id);
                tracing::info!(
                    watch_id = %Uuid::from_bytes(watch_id),
                    root = %root.display(),
                    "rebound AWACS watch to moved subvolume path"
                );
            }
        }
        let (watch_id, grant_id) = existing.ok_or_else(|| {
            unavailable(format!(
                "{} is not initialized; run awacs init {} first",
                root.display(),
                root.display(),
            ))
        })?;
        match facade.view_binding(watch_id) {
            Some(binding) if binding.root_path.as_slice() == root_bytes => {}
            Some(_) => {
                return Err(ScanError::new(
                    ScanErrorKind::Unauthorized,
                    "AWACS scan root conflicts with its active facade binding",
                ));
            }
            None => facade
                .activate(watch_id, grant_id, &root)
                .map_err(|err| other(format!("activate AWACS scan root: {err}")))?,
        }
        Ok((root, watch_id))
    }

    /// Debug-build integration hook used to prove the client traverses the
    /// immutable scan root after the live root changes. The hook is inert
    /// unless an explicit control directory and one-shot arm file exist.
    #[cfg(debug_assertions)]
    fn maybe_mutate_live_after_begin(&self, live_root: &std::path::Path) -> Result<(), ScanError> {
        let Some(control_dir) = &self.test_control_dir else {
            return Ok(());
        };
        let arm_path = control_dir.join("mutate-live-after-begin");
        let relative = match std::fs::read_to_string(&arm_path) {
            Ok(relative) => PathBuf::from(relative.trim_end()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(other(format!("read AWACS scan test hook: {err}"))),
        };
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(other(
                "AWACS scan test hook path must be relative and normal",
            ));
        }
        std::fs::write(
            live_root.join(&relative),
            b"live mutation after BeginScan\n",
        )
        .map_err(|err| other(format!("apply AWACS scan test hook: {err}")))?;
        std::fs::remove_file(&arm_path)
            .map_err(|err| other(format!("disarm AWACS scan test hook: {err}")))?;
        std::fs::write(
            control_dir.join("mutation-complete"),
            relative.to_string_lossy().as_bytes(),
        )
        .map_err(|err| other(format!("record AWACS scan test hook completion: {err}")))?;
        Ok(())
    }

    #[cfg(debug_assertions)]
    fn scan_ttl_ns(&self) -> i64 {
        let Some(control_dir) = &self.test_control_dir else {
            return SCAN_TTL_NS;
        };
        if std::fs::remove_file(control_dir.join("short-lease")).is_ok() {
            TEST_SHORT_SCAN_TTL_NS
        } else {
            SCAN_TTL_NS
        }
    }

    #[cfg(not(debug_assertions))]
    fn scan_ttl_ns(&self) -> i64 {
        SCAN_TTL_NS
    }

    #[cfg(debug_assertions)]
    fn maybe_reject_renew_for_test(&self) -> Result<(), ScanError> {
        let Some(control_dir) = &self.test_control_dir else {
            return Ok(());
        };
        if std::fs::remove_file(control_dir.join("reject-renew")).is_ok() {
            Err(ScanError::new(
                ScanErrorKind::LeaseExpired,
                "AWACS scan test hook rejected renewal",
            ))
        } else {
            Ok(())
        }
    }

    /// Reclaims expired scan sessions and bounded finish tombstones.
    ///
    /// The durable query lease also expires in the manager, but dropping the
    /// handler-side prepared response prevents abandoned client connections
    /// from accumulating process memory forever.
    fn expire_sessions(&mut self, now_ns: i64) -> Result<(), ScanError> {
        self.finished_sessions
            .retain(|_, expires_ns| *expires_ns > now_ns);
        let expired = self
            .sessions
            .iter()
            .filter(|(_, session)| session.expires_ns <= now_ns)
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        if expired.is_empty() {
            return Ok(());
        }
        let mut facade = self
            .facade
            .lock()
            .map_err(|_| other("AWACS facade lock poisoned"))?;
        for session_id in expired {
            if let Some(session) = self.sessions.remove(&session_id) {
                // Expiry already makes the durable fence unusable. Releasing
                // it eagerly is best-effort cleanup; the manager's lease
                // expiry remains the authoritative reclamation path.
                if !session.logical_git_consumer {
                    let _ = facade.finish_consumer_baseline(session.baseline_owner_id, false);
                }
                let _ = facade.finish_query_response(session.prepared);
            }
        }
        Ok(())
    }
}

impl DirectScanHandler {
    fn initial_baseline(&mut self, live_root: &Path) -> Result<SnapshotBaseline, ScanError> {
        let mut facade = self
            .facade
            .lock()
            .map_err(|_| other("AWACS facade lock poisoned"))?;
        let (_requested_root, watch_id) = self.ensure_registered_root(&mut facade, live_root)?;
        facade
            .initial_scan_baseline(watch_id)
            .map_err(|err| other(format!("load initial AWACS scan baseline: {err}")))
    }

    fn observe_persisted_git_index(
        &mut self,
        live_root: &Path,
        expected_baseline: &SnapshotBaseline,
    ) -> Result<(), ScanError> {
        let mut facade = self
            .facade
            .lock()
            .map_err(|_| other("AWACS facade lock poisoned"))?;
        let (root, watch_id) = self.ensure_registered_root(&mut facade, live_root)?;
        validate_persisted_git_index(&root, expected_baseline)?;
        if !facade
            .observe_logical_git_baseline(watch_id, expected_baseline, unix_time_ns()?)
            .map_err(|err| other(format!("observe published AWACS Git index: {err}")))?
        {
            return Err(ScanError::new(
                ScanErrorKind::InvalidPreviousBaseline,
                "published AWACS Git index cursor is no longer retained",
            ));
        }
        Ok(())
    }

    fn begin_scan(&mut self, request: BeginScanRequest) -> Result<PreparedScanLease, ScanError> {
        let begin_started = Instant::now();
        let now_ns = unix_time_ns()?;
        let ttl_ns = self.scan_ttl_ns();
        self.expire_sessions(now_ns)?;
        let requested_previous_baseline = request.previous_baseline.is_some();
        let facade_lock_started = Instant::now();
        let mut facade = self
            .facade
            .lock()
            .map_err(|_| other("AWACS facade lock poisoned"))?;
        tracing::info!(
            elapsed_ms = facade_lock_started.elapsed().as_millis() as u64,
            root = %request.live_root.display(),
            "direct scan facade lock acquired"
        );
        // Every direct scan remains bound to the caller's exact canonical
        // root.
        let registration_started = Instant::now();
        let (_requested_root, watch_id) =
            self.ensure_registered_root(&mut facade, &request.live_root)?;
        tracing::info!(
            elapsed_ms = registration_started.elapsed().as_millis() as u64,
            root = %request.live_root.display(),
            watch_id = %Uuid::from_bytes(watch_id),
            "direct scan root registered"
        );
        let baseline_reconcile_started = Instant::now();
        let logical_git_consumer = request.baseline_owner_id == watch_id;
        let previous_baseline = if logical_git_consumer
            && request
                .previous_baseline
                .as_ref()
                .is_some_and(|baseline| !baseline.continuity_token.is_empty())
        {
            let baseline = request
                .previous_baseline
                .as_ref()
                .expect("checked Git logical baseline");
            // The hook allows full invalidation; internal index seeding does
            // not. Writers separately verify their published index before
            // recording an observation; issuing this response proves nothing.
            // Record it before issuing the next cut, whose output may never
            // be committed to the index (e.g. GIT_OPTIONAL_LOCKS=0).
            let retained = if request.allow_full_invalidation {
                facade.observe_logical_git_baseline(watch_id, baseline, unix_time_ns()?)
            } else {
                facade.validate_logical_git_baseline(watch_id, baseline)
            }
            .map_err(|err| other(format!("validate AWACS Git logical baseline: {err}")))?;
            retained.then_some(baseline)
        } else if facade
            .reconcile_consumer_baseline(
                watch_id,
                request.baseline_owner_id,
                request.previous_baseline.as_ref(),
            )
            .map_err(|err| other(format!("reconcile AWACS consumer baseline: {err}")))?
        {
            request.previous_baseline.as_ref()
        } else {
            None
        };
        tracing::info!(
            elapsed_ms = baseline_reconcile_started.elapsed().as_millis() as u64,
            root = %request.live_root.display(),
            reused_previous_baseline = previous_baseline.is_some(),
            "direct scan consumer baseline reconciled"
        );
        if requested_previous_baseline
            && previous_baseline.is_none()
            && !(logical_git_consumer && request.allow_full_invalidation)
        {
            return Err(ScanError::new(
                ScanErrorKind::InvalidPreviousBaseline,
                "AWACS could not retain the requested previous baseline",
            ));
        }
        if previous_baseline.is_none() && !request.allow_full_invalidation {
            return Err(full_scan_required(
                "AWACS scan has no retained baseline; use an explicit baseline rebuild",
            ));
        }
        // Root registration and the first cut can lazily index a large
        // checkout. While this synchronous Begin request still owns the
        // socket, keep its preparation fence non-expiring. Once the response
        // is ready below, renew it to the ordinary bounded session TTL.
        let prepare_now_ns = unix_time_ns()?;
        let query_prepare_started = Instant::now();
        let prepared = facade
            .prepare_scan_query(
                watch_id,
                previous_baseline,
                self.requester_uid,
                self.requester_gid,
                prepare_now_ns,
                i64::MAX,
                logical_git_consumer && request.allow_full_invalidation,
            )
            .map_err(|err| other(format!("prepare AWACS scan cut: {err}")))?;
        tracing::info!(
            elapsed_ms = query_prepare_started.elapsed().as_millis() as u64,
            root = %request.live_root.display(),
            watch_id = %Uuid::from_bytes(watch_id),
            sequence = prepared.result.sequence,
            projected_path_count = prepared.result.projection.paths.len(),
            projected_prefix_count = prepared.result.projection.prefixes.len(),
            fresh_instance = prepared.result.projection.fresh_instance,
            "direct scan query prepared"
        );
        let snapshot_path = match facade.prepared_snapshot_path(&prepared) {
            Ok(path) => path,
            Err(err) => {
                let _ = facade.finish_query_response(prepared);
                return Err(other(format!("load AWACS scan snapshot path: {err}")));
            }
        };
        let scan_root = match File::open(&snapshot_path) {
            Ok(file) => file,
            Err(err) => {
                let _ = facade.finish_query_response(prepared);
                return Err(other(format!("open AWACS scan snapshot: {err}")));
            }
        };
        let identity = match scan_identity(&scan_root) {
            Ok(identity) => identity,
            Err(err) => {
                let _ = facade.finish_query_response(prepared);
                return Err(err);
            }
        };
        // Lazy root registration and the first immutable cut can be
        // expensive. Renew from a fresh wall-clock sample so the durable
        // fence and the boot-clock deadline advertised below cover the same
        // full TTL after Begin has finished preparing its response.
        let lease_now_ns = match unix_time_ns() {
            Ok(now_ns) => now_ns,
            Err(err) => {
                let _ = facade.finish_query_response(prepared);
                return Err(err);
            }
        };
        if let Err(err) = facade.renew_query_response(&prepared, lease_now_ns, ttl_ns) {
            let _ = facade.finish_query_response(prepared);
            return Err(other(format!("extend AWACS scan lease: {err}")));
        }
        let session_id = Uuid::new_v4().as_bytes().to_vec();
        let invalidation =
            match direct_invalidation(&prepared.result.projection, request.allow_full_invalidation)
            {
                Ok(invalidation) => invalidation,
                Err(err) => {
                    let _ = facade.finish_query_response(prepared);
                    return Err(err);
                }
            };
        let continuity_token = match facade.direct_scan_continuity_token(&prepared) {
            Ok(token) => token,
            Err(err) => {
                let _ = facade.finish_query_response(prepared);
                return Err(other(format!("encode AWACS continuity token: {err}")));
            }
        };
        #[cfg(debug_assertions)]
        if let Err(err) = self.maybe_mutate_live_after_begin(&_requested_root) {
            let _ = facade.finish_query_response(prepared);
            return Err(err);
        }
        let expires_ns = lease_now_ns
            .checked_add(ttl_ns)
            .ok_or_else(|| other("AWACS scan lease expiration overflow"))?;
        if logical_git_consumer {
            // The target remains physically pinned by this query lease, while
            // Git's eventual on-disk cursor names a logical journal position.
            // Incoming observations protect replay; issuing a concurrent
            // response must not advance that observation or replace another
            // process's exclusive consumer pin.
            if let Err(err) = facade.release_consumer_baseline(request.baseline_owner_id) {
                let _ = facade.finish_query_response(prepared);
                return Err(other(format!(
                    "release superseded physical AWACS Git baseline: {err}"
                )));
            }
        } else if let Err(err) =
            facade.stage_consumer_baseline(&prepared, request.baseline_owner_id)
        {
            let _ = facade.finish_query_response(prepared);
            return Err(other(format!(
                "retain pending AWACS consumer baseline: {err}"
            )));
        }
        self.sessions.insert(
            session_id.clone(),
            ActiveScanSession {
                prepared,
                baseline_owner_id: request.baseline_owner_id,
                logical_git_consumer,
                expires_ns,
            },
        );
        let lease = PreparedScanLease {
            session_id,
            next_baseline: SnapshotBaseline {
                identity,
                continuity_token,
                // This opaque token names the stable consumer owner. It is
                // not a path or a mutable commit id.
                retention_token: request.baseline_owner_id.to_vec(),
            },
            invalidation,
            expires_boottime_ns: crate::scan::boottime_now_ns().saturating_add(ttl_ns as u64),
            scan_root,
        };
        tracing::info!(
            elapsed_ms = begin_started.elapsed().as_millis() as u64,
            root = %request.live_root.display(),
            watch_id = %Uuid::from_bytes(watch_id),
            "direct scan handler completed begin"
        );
        Ok(lease)
    }

    fn renew_scan(&mut self, session_id: &[u8]) -> Result<(), ScanError> {
        let now_ns = unix_time_ns()?;
        self.expire_sessions(now_ns)?;
        #[cfg(debug_assertions)]
        self.maybe_reject_renew_for_test()?;
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| ScanError::new(ScanErrorKind::LeaseExpired, "unknown AWACS session"))?;
        let mut facade = self
            .facade
            .lock()
            .map_err(|_| other("AWACS facade lock poisoned"))?;
        facade
            .renew_query_response_direct(&session.prepared, now_ns, SCAN_TTL_NS)
            .map_err(|err| ScanError::new(ScanErrorKind::LeaseExpired, err.to_string()))?;
        session.expires_ns = now_ns
            .checked_add(SCAN_TTL_NS)
            .ok_or_else(|| other("AWACS scan lease expiration overflow"))?;
        Ok(())
    }

    fn promote_scan(&mut self, session_id: &[u8]) -> Result<(), ScanError> {
        let now_ns = unix_time_ns()?;
        self.expire_sessions(now_ns)?;
        self.sessions
            .get(session_id)
            .ok_or_else(|| ScanError::new(ScanErrorKind::LeaseExpired, "unknown AWACS session"))?;
        Ok(())
    }

    fn finish_scan(&mut self, session_id: &[u8], outcome: ScanOutcome) -> Result<(), ScanError> {
        timed_scan_phase("finish scan", || {
            self.finish_scan_inner(session_id, outcome)
        })
    }

    fn finish_scan_inner(
        &mut self,
        session_id: &[u8],
        outcome: ScanOutcome,
    ) -> Result<(), ScanError> {
        let now_ns = unix_time_ns()?;
        timed_scan_phase("expire finish sessions", || self.expire_sessions(now_ns))?;
        if self.finished_sessions.contains_key(session_id) {
            return Ok(());
        }
        let session = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| ScanError::new(ScanErrorKind::LeaseExpired, "unknown AWACS session"))?;
        let mut facade = timed_scan_phase("finish facade lock", || {
            self.facade
                .lock()
                .map_err(|_| other("AWACS facade lock poisoned"))
        })?;
        if !session.logical_git_consumer {
            if let Err(err) = timed_scan_phase("finish consumer baseline", || {
                facade
                    .finish_consumer_baseline(
                        session.baseline_owner_id,
                        outcome == ScanOutcome::Committed,
                    )
                    .map_err(|err| other(format!("finish AWACS consumer baseline: {err}")))
            }) {
                let _ = timed_scan_phase("abort query response", || {
                    facade
                        .finish_query_response(session.prepared)
                        .map(|_| ())
                        .map_err(|err| other(format!("abort AWACS scan lease: {err}")))
                });
                return Err(err);
            }
        }
        timed_scan_phase("finish query response", || {
            facade
                .finish_query_response_direct(session.prepared)
                .map(|_| ())
                .map_err(|err| other(format!("finish AWACS scan lease: {err}")))
        })?;
        // Once this in-process scan drops its query fence, run the shared
        // consumer-aware retention pass. JJ owns a durable physical endpoint;
        // Git observations protect a logical replay interval independently of
        // response completion: Finish does not prove an index was written.
        let gc_started = Instant::now();
        let gc_result = facade
            .service_mut()
            .garbage_collect_direct(now_ns, 64)
            .map_err(|err| other(format!("collect completed AWACS scan: {err}")));
        record_scan_phase(
            "garbage collect direct",
            gc_started,
            gc_result.is_ok(),
            gc_result.as_ref().ok().copied(),
        );
        gc_result?;
        let tombstone_expires_ns = now_ns
            .checked_add(SCAN_TTL_NS)
            .ok_or_else(|| other("AWACS finish tombstone expiration overflow"))?;
        self.finished_sessions
            .insert(session_id.to_vec(), tombstone_expires_ns);
        Ok(())
    }

    fn release_baseline(&mut self, baseline_owner_id: [u8; 16]) -> Result<(), ScanError> {
        timed_scan_phase("release baseline", || {
            self.release_baseline_inner(baseline_owner_id)
        })
    }

    fn release_baseline_inner(&mut self, baseline_owner_id: [u8; 16]) -> Result<(), ScanError> {
        let now_ns = unix_time_ns()?;
        timed_scan_phase("expire release sessions", || self.expire_sessions(now_ns))?;
        let mut facade = timed_scan_phase("release facade lock", || {
            self.facade
                .lock()
                .map_err(|_| other("AWACS facade lock poisoned"))
        })?;
        timed_scan_phase("release consumer baseline", || {
            facade
                .release_consumer_baseline(baseline_owner_id)
                .map_err(|err| other(format!("release AWACS consumer baseline: {err}")))
        })?;
        // A mode switch should not leave an avoidable stack of read-only
        // snapshots behind until the next direct caller.
        let gc_started = Instant::now();
        let gc_result = facade
            .service_mut()
            .garbage_collect_direct(now_ns, 64)
            .map_err(|err| other(format!("collect released AWACS snapshots: {err}")));
        record_scan_phase(
            "garbage collect released baseline",
            gc_started,
            gc_result.is_ok(),
            gc_result.as_ref().ok().copied(),
        );
        gc_result?;
        Ok(())
    }
}

fn validate_persisted_git_index(
    live_root: &Path,
    expected_baseline: &SnapshotBaseline,
) -> Result<(), ScanError> {
    let git_dir = git_admin_directory(live_root)
        .map_err(|err| other(format!("resolve published Git index: {err}")))?
        .ok_or_else(|| other("AWACS root has no Git administrative directory"))?;
    let index = inspect_git_index(&git_dir.join("index"))
        .map_err(|err| other(format!("read published Git index: {err}")))?;
    if index.checksum != IndexChecksum::Valid {
        return Err(other("published Git index omits its content checksum"));
    }
    match index.fsmonitor {
        GitFsmonitorStatus::Baseline(baseline) if baseline == *expected_baseline => Ok(()),
        _ => Err(other(
            "published Git index does not contain the expected AWACS cursor",
        )),
    }
}

fn timed_scan_phase<T>(
    phase: &'static str,
    action: impl FnOnce() -> Result<T, ScanError>,
) -> Result<T, ScanError> {
    let _span = tracing::info_span!("direct_scan_phase", phase, pid = std::process::id()).entered();
    let started = Instant::now();
    let result = action();
    record_scan_phase(phase, started, result.is_ok(), None);
    result
}

fn record_scan_phase(
    phase: &'static str,
    started: Instant,
    success: bool,
    deleted_snapshots: Option<usize>,
) {
    let elapsed = started.elapsed();
    tracing::info!(
        phase,
        pid = std::process::id(),
        elapsed_ms = elapsed.as_millis() as u64,
        elapsed_us = elapsed.as_micros() as u64,
        success,
        deleted_snapshots,
        "AWACS direct scan phase completed"
    );
}

fn direct_invalidation(
    projection: &crate::compat::Projection,
    allow_full_invalidation: bool,
) -> Result<Invalidation, ScanError> {
    if projection.fresh_instance {
        return if allow_full_invalidation {
            Ok(Invalidation::Full)
        } else {
            Err(full_scan_required(
                "AWACS could not prove an exact delta from the retained baseline",
            ))
        };
    }
    let normalize = |path: &[u8]| {
        // Index events are already repository-relative. Accept the old
        // slash-prefixed representation as a compatibility input, but do not
        // turn a valid relative delta into a full invalidation.
        let path = path.strip_prefix(b"/").unwrap_or(path);
        if path.is_empty() {
            None
        } else {
            Some(path.to_vec())
        }
    };
    if !projection.prefixes.is_empty() {
        let mut prefixes = Vec::with_capacity(projection.paths.len() + projection.prefixes.len());
        for path in projection.prefixes.iter().chain(&projection.paths) {
            let Some(path) = normalize(path) else {
                return if allow_full_invalidation {
                    Ok(Invalidation::Full)
                } else {
                    Err(full_scan_required(
                        "AWACS delta contains a repository-root prefix",
                    ))
                };
            };
            prefixes.push(path);
        }
        return Ok(Invalidation::Prefixes(prefixes));
    }
    let mut paths = Vec::with_capacity(projection.paths.len());
    for path in &projection.paths {
        let Some(path) = normalize(path) else {
            return if allow_full_invalidation {
                Ok(Invalidation::Full)
            } else {
                Err(full_scan_required(
                    "AWACS delta contains a repository-root path",
                ))
            };
        };
        paths.push(path);
    }
    Ok(Invalidation::ExactPaths(paths))
}

fn scan_identity(scan_root: &File) -> Result<SnapshotIdentity, ScanError> {
    let filesystem = filesystem_info(scan_root.as_fd())
        .map_err(|err| other(format!("inspect AWACS scan filesystem: {err}")))?;
    let subvolume = subvolume_info(scan_root.as_fd())
        .map_err(|err| other(format!("inspect AWACS scan subvolume: {err}")))?;
    Ok(SnapshotIdentity {
        filesystem_uuid: filesystem.fs_uuid,
        subvolume_uuid: subvolume.uuid,
        read_only: subvolume.readonly(),
    })
}

fn unix_time_ns() -> Result<i64, ScanError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| other(format!("read AWACS wall clock: {err}")))?;
    i64::try_from(duration.as_nanos()).map_err(|_| other("AWACS wall clock overflow"))
}

fn unavailable(message: impl Into<String>) -> ScanError {
    ScanError::new(ScanErrorKind::Unavailable, message)
}

fn other(message: impl Into<String>) -> ScanError {
    ScanError::new(ScanErrorKind::Other, message)
}

fn full_scan_required(message: impl Into<String>) -> ScanError {
    ScanError::new(ScanErrorKind::FullScanRequired, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persisted_index_baseline() -> SnapshotBaseline {
        SnapshotBaseline {
            identity: SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:2:test".to_vec(),
            retention_token: vec![3; 16],
        }
    }

    fn persisted_index_bytes(token: Option<&str>, hash_len: usize) -> Vec<u8> {
        let mut bytes = b"DIRC".to_vec();
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        if let Some(token) = token {
            let mut payload = 2_u32.to_be_bytes().to_vec();
            payload.extend_from_slice(token.as_bytes());
            payload.push(0);
            bytes.extend_from_slice(b"FSMN");
            bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&payload);
        }
        let checksum = crate::status::git_index_checksum(&bytes, hash_len).unwrap();
        bytes.extend_from_slice(&checksum);
        bytes
    }

    #[test]
    fn persisted_git_index_readback_requires_exact_cursor_and_checksum() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let expected = persisted_index_baseline();
        assert!(validate_persisted_git_index(root, &expected).is_err());
        std::fs::create_dir(root.join(".git")).unwrap();
        assert!(validate_persisted_git_index(root, &expected).is_err());
        let index = root.join(".git/index");
        let token = crate::status::encode_git_fsmonitor_token(&expected).unwrap();
        for hash_len in [20, 32] {
            for absent_or_invalid in [None, Some("123456789"), Some("awacs-git-v2:invalid")] {
                std::fs::write(&index, persisted_index_bytes(absent_or_invalid, hash_len)).unwrap();
                assert!(validate_persisted_git_index(root, &expected).is_err());
            }
            let mut bytes = persisted_index_bytes(Some(&token), hash_len);
            std::fs::write(&index, &bytes).unwrap();
            validate_persisted_git_index(root, &expected).unwrap();

            let mut different = expected.clone();
            different.continuity_token.push(b'x');
            assert!(validate_persisted_git_index(root, &different).is_err());

            *bytes.last_mut().unwrap() ^= 1;
            std::fs::write(&index, &bytes).unwrap();
            assert!(validate_persisted_git_index(root, &expected).is_err());

            let checksum_start = bytes.len() - hash_len;
            bytes[checksum_start..].fill(0);
            std::fs::write(&index, &bytes).unwrap();
            let error = validate_persisted_git_index(root, &expected).unwrap_err();
            assert!(error.to_string().contains("omits its content checksum"));
        }
    }

    #[test]
    fn persisted_git_index_readback_uses_linked_worktree_admin_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("child");
        let common_dir = directory.path().join("repo/.git");
        let git_dir = common_dir.join("worktrees/child");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(root.join(".git"), b"gitdir: ../repo/.git/worktrees/child\n").unwrap();
        let expected = persisted_index_baseline();
        let token = crate::status::encode_git_fsmonitor_token(&expected).unwrap();
        // A valid main-worktree index cannot stand in for this linked root.
        std::fs::write(
            common_dir.join("index"),
            persisted_index_bytes(Some(&token), 20),
        )
        .unwrap();
        assert!(validate_persisted_git_index(&root, &expected).is_err());
        std::fs::write(
            git_dir.join("index"),
            persisted_index_bytes(Some(&token), 20),
        )
        .unwrap();
        validate_persisted_git_index(&root, &expected).unwrap();
        std::fs::write(git_dir.join("index"), persisted_index_bytes(None, 20)).unwrap();
        assert!(validate_persisted_git_index(&root, &expected).is_err());
    }

    #[test]
    fn direct_invalidation_uses_repo_relative_exact_paths() {
        let projection = crate::compat::Projection {
            fresh_instance: false,
            paths: vec![b"dir/file".to_vec(), b"name".to_vec()],
            prefixes: Vec::new(),
        };
        assert_eq!(
            direct_invalidation(&projection, false).unwrap(),
            Invalidation::ExactPaths(vec![b"dir/file".to_vec(), b"name".to_vec()])
        );
        assert_eq!(
            direct_invalidation(
                &crate::compat::Projection {
                    fresh_instance: false,
                    paths: vec![b"/compat".to_vec()],
                    prefixes: Vec::new(),
                },
                false
            )
            .unwrap(),
            Invalidation::ExactPaths(vec![b"compat".to_vec()])
        );
        let error = direct_invalidation(
            &crate::compat::Projection {
                fresh_instance: false,
                paths: vec![b"/".to_vec()],
                prefixes: Vec::new(),
            },
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ScanErrorKind::FullScanRequired);
        assert_eq!(
            direct_invalidation(
                &crate::compat::Projection {
                    fresh_instance: false,
                    paths: vec![b"/".to_vec()],
                    prefixes: Vec::new(),
                },
                true,
            )
            .unwrap(),
            Invalidation::Full
        );
        assert_eq!(
            direct_invalidation(
                &crate::compat::Projection {
                    fresh_instance: false,
                    paths: vec![b"exact".to_vec()],
                    prefixes: vec![b"moved".to_vec()],
                },
                false
            )
            .unwrap(),
            Invalidation::Prefixes(vec![b"moved".to_vec(), b"exact".to_vec()])
        );
    }

    #[test]
    fn direct_invalidation_refuses_fresh_projection_without_explicit_permission() {
        let projection = crate::compat::Projection {
            fresh_instance: true,
            paths: vec![b"/".to_vec()],
            prefixes: Vec::new(),
        };
        let error = direct_invalidation(&projection, false).unwrap_err();
        assert_eq!(error.kind(), ScanErrorKind::FullScanRequired);
        assert_eq!(
            direct_invalidation(&projection, true).unwrap(),
            Invalidation::Full,
            "Git can safely recover an expired logical cursor with its standard full invalidation"
        );
    }
}
