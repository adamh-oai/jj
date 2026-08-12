//! Direct scan sessions backed by the synchronized snapshot facade.

use crate::btrfs::{filesystem_info, subvolume_info};
use crate::facade::{FacadeService, PreparedQueryResult};
use crate::scan::{
    BeginScanRequest, Invalidation, ScanError, ScanErrorKind, ScanOutcome, ScanRequestHandler,
    ServerScanLease, SnapshotIdentity,
};
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsFd;
#[cfg(debug_assertions)]
use std::path::Component;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SCAN_TTL_NS: i64 = 300_000_000_000;
#[cfg(debug_assertions)]
const TEST_SHORT_SCAN_TTL_NS: i64 = 300_000_000;

/// Durable direct-scan handler sharing one already-registered facade.
///
/// Direct cursors wrap the existing authenticated cut claims under a distinct
/// authenticated scan domain. Exact replay is accepted only when the facade
/// can retain that same cut sequence and target snapshot UUID.
pub struct FacadeScanHandler {
    facade: Arc<Mutex<FacadeService>>,
    expected_live_root: PathBuf,
    watch_id: [u8; 16],
    requester_uid: u32,
    requester_gid: u32,
    sessions: HashMap<Vec<u8>, ActiveScanSession>,
    finished_sessions: HashMap<Vec<u8>, i64>,
    #[cfg(debug_assertions)]
    test_control_dir: Option<PathBuf>,
}

struct ActiveScanSession {
    prepared: PreparedQueryResult,
    expires_ns: i64,
}

impl FacadeScanHandler {
    /// Creates a direct handler for one registered live root.
    pub fn new(
        facade: Arc<Mutex<FacadeService>>,
        expected_live_root: PathBuf,
        watch_id: [u8; 16],
        requester_uid: u32,
        requester_gid: u32,
    ) -> Self {
        Self {
            facade,
            expected_live_root,
            watch_id,
            requester_uid,
            requester_gid,
            sessions: HashMap::new(),
            finished_sessions: HashMap::new(),
            #[cfg(debug_assertions)]
            test_control_dir: std::env::var_os("BTRFS_AWACS_SCAN_TEST_CONTROL_DIR")
                .map(PathBuf::from),
        }
    }

    /// Debug-build integration hook used to prove the client traverses the
    /// immutable scan root after the live root changes. The hook is inert
    /// unless an explicit control directory and one-shot arm file exist.
    #[cfg(debug_assertions)]
    fn maybe_mutate_live_after_begin(&self) -> Result<(), ScanError> {
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
            self.expected_live_root.join(&relative),
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
            .filter_map(|(session_id, session)| {
                (session.expires_ns <= now_ns).then(|| session_id.clone())
            })
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
                let _ = facade.finish_query_response(session.prepared);
            }
        }
        Ok(())
    }
}

impl ScanRequestHandler for FacadeScanHandler {
    fn begin_scan(&mut self, request: BeginScanRequest) -> Result<ServerScanLease, ScanError> {
        let requested_root = std::fs::canonicalize(&request.live_root)
            .map_err(|err| unavailable(format!("canonicalize AWACS scan root: {err}")))?;
        if requested_root != self.expected_live_root {
            return Err(ScanError::new(
                ScanErrorKind::Unauthorized,
                "AWACS scan root does not match the registered live root",
            ));
        }
        let now_ns = unix_time_ns()?;
        let ttl_ns = self.scan_ttl_ns();
        self.expire_sessions(now_ns)?;
        let mut facade = self
            .facade
            .lock()
            .map_err(|_| other("AWACS facade lock poisoned"))?;
        let prepared = facade
            .prepare_scan_query(
                self.watch_id,
                request.previous_cursor.as_deref(),
                self.requester_uid,
                self.requester_gid,
                now_ns,
            )
            .map_err(|err| other(format!("prepare AWACS scan cut: {err}")))?;
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
        if let Err(err) = facade.renew_query_response(&prepared, now_ns, ttl_ns) {
            let _ = facade.finish_query_response(prepared);
            return Err(other(format!("extend AWACS scan lease: {err}")));
        }
        let session_id = Uuid::new_v4().as_bytes().to_vec();
        let invalidation = direct_invalidation(&prepared.result.projection);
        let cursor = match facade.direct_scan_cursor(&prepared) {
            Ok(cursor) => cursor,
            Err(err) => {
                let _ = facade.finish_query_response(prepared);
                return Err(other(format!("encode AWACS direct cursor: {err}")));
            }
        };
        #[cfg(debug_assertions)]
        if let Err(err) = self.maybe_mutate_live_after_begin() {
            let _ = facade.finish_query_response(prepared);
            return Err(err);
        }
        let expires_ns = now_ns
            .checked_add(ttl_ns)
            .ok_or_else(|| other("AWACS scan lease expiration overflow"))?;
        self.sessions.insert(
            session_id.clone(),
            ActiveScanSession {
                prepared,
                expires_ns,
            },
        );
        Ok(ServerScanLease {
            session_id,
            cursor,
            invalidation,
            expires_boottime_ns: crate::scan::boottime_now_ns().saturating_add(ttl_ns as u64),
            identity,
            scan_root,
        })
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
        self.facade
            .lock()
            .map_err(|_| other("AWACS facade lock poisoned"))?
            .renew_query_response(&session.prepared, now_ns, SCAN_TTL_NS)
            .map_err(|err| ScanError::new(ScanErrorKind::LeaseExpired, err.to_string()))?;
        session.expires_ns = now_ns
            .checked_add(SCAN_TTL_NS)
            .ok_or_else(|| other("AWACS scan lease expiration overflow"))?;
        Ok(())
    }

    fn finish_scan(&mut self, session_id: &[u8], _outcome: ScanOutcome) -> Result<(), ScanError> {
        let now_ns = unix_time_ns()?;
        self.expire_sessions(now_ns)?;
        if self.finished_sessions.contains_key(session_id) {
            return Ok(());
        }
        let session = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| ScanError::new(ScanErrorKind::LeaseExpired, "unknown AWACS session"))?;
        self.facade
            .lock()
            .map_err(|_| other("AWACS facade lock poisoned"))?
            .finish_query_response(session.prepared)
            .map(|_| ())
            .map_err(|err| other(format!("finish AWACS scan lease: {err}")))?;
        let tombstone_expires_ns = now_ns
            .checked_add(SCAN_TTL_NS)
            .ok_or_else(|| other("AWACS finish tombstone expiration overflow"))?;
        self.finished_sessions
            .insert(session_id.to_vec(), tombstone_expires_ns);
        Ok(())
    }
}

fn direct_invalidation(projection: &crate::compat::Projection) -> Invalidation {
    if projection.fresh_instance {
        return Invalidation::Full;
    }
    let mut paths = Vec::with_capacity(projection.paths.len());
    for path in &projection.paths {
        let Some(path) = path.strip_prefix(b"/") else {
            return Invalidation::Full;
        };
        if path.is_empty() {
            return Invalidation::Full;
        }
        paths.push(path.to_vec());
    }
    Invalidation::ExactPaths(paths)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_invalidation_uses_repo_relative_exact_paths() {
        let projection = crate::compat::Projection {
            fresh_instance: false,
            paths: vec![b"/dir/file".to_vec(), b"/name".to_vec()],
        };
        assert_eq!(
            direct_invalidation(&projection),
            Invalidation::ExactPaths(vec![b"dir/file".to_vec(), b"name".to_vec()])
        );
        assert_eq!(
            direct_invalidation(&crate::compat::Projection {
                fresh_instance: false,
                paths: vec![b"/".to_vec()],
            }),
            Invalidation::Full
        );
    }
}
