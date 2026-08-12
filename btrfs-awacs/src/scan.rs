//! Transport-independent client API for immutable snapshot scans.
//!
//! Jujutsu embeds AWACS and uses the direct in-process implementation. Every
//! successful begin keeps one read-only directory handle alive until the
//! lease is finished.

use std::error::Error;
use std::fmt;
use std::fs::File;
use std::mem::zeroed;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotBaseline {
    pub identity: SnapshotIdentity,
    pub continuity_token: Vec<u8>,
    pub retention_token: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginScanRequest {
    pub live_root: PathBuf,
    pub baseline_owner_id: [u8; 16],
    pub previous_baseline: Option<SnapshotBaseline>,
    /// Grants the caller permission to receive a whole-tree invalidation.
    ///
    /// Normal consumers must leave this false: without an exact retained
    /// baseline and exact delta proof, they fail instead of silently widening
    /// to a full scan. JJ sets this only while explicitly establishing or
    /// rebuilding a snapshot-backed baseline.
    pub allow_full_invalidation: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Invalidation {
    Full,
    ExactPaths(Vec<Vec<u8>>),
    Prefixes(Vec<Vec<u8>>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotIdentity {
    pub filesystem_uuid: [u8; 16],
    pub subvolume_uuid: [u8; 16],
    pub read_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanOutcome {
    Committed,
    Aborted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanErrorKind {
    UnsupportedFilesystem,
    Unavailable,
    InvalidPreviousBaseline,
    FullScanRequired,
    LeaseExpired,
    Unauthorized,
    MalformedResponse,
    Other,
}

#[derive(Debug)]
pub struct ScanError {
    kind: ScanErrorKind,
    message: String,
}

impl ScanError {
    pub fn new(kind: ScanErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> ScanErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ScanError {}

pub trait ScanSession: Send {
    fn renew(&mut self) -> Result<(), ScanError>;
    fn promote(&mut self) -> Result<(), ScanError>;
    fn finish(&mut self, outcome: ScanOutcome) -> Result<(), ScanError>;
}

pub struct SnapshotLease {
    pub next_baseline: SnapshotBaseline,
    pub invalidation: Invalidation,
    pub expires_boottime_ns: u64,
    scan_root: File,
    session: Box<dyn ScanSession>,
    finished_outcome: Option<ScanOutcome>,
}

impl SnapshotLease {
    pub fn new(
        next_baseline: SnapshotBaseline,
        invalidation: Invalidation,
        expires_boottime_ns: u64,
        scan_root: File,
        session: Box<dyn ScanSession>,
    ) -> Self {
        Self {
            next_baseline,
            invalidation,
            expires_boottime_ns,
            scan_root,
            session,
            finished_outcome: None,
        }
    }

    pub fn scan_root(&self) -> &File {
        &self.scan_root
    }

    pub fn renew(&mut self) -> Result<(), ScanError> {
        self.session.renew()
    }

    pub fn promote(&mut self) -> Result<(), ScanError> {
        self.session.promote()
    }

    pub fn renewal_interval(&self) -> Duration {
        const MAX_RENEW_INTERVAL_NS: u64 = 60_000_000_000;
        let ttl_ns = self.expires_boottime_ns.saturating_sub(boottime_now_ns());
        Duration::from_nanos((ttl_ns / 3).clamp(1, MAX_RENEW_INTERVAL_NS))
    }

    pub fn finish(&mut self, outcome: ScanOutcome) -> Result<(), ScanError> {
        if let Some(previous) = self.finished_outcome {
            if previous == outcome {
                return Ok(());
            }
            return Err(ScanError::new(
                ScanErrorKind::Other,
                "AWACS scan lease was already finished with a different outcome",
            ));
        }
        self.session.finish(outcome)?;
        self.finished_outcome = Some(outcome);
        Ok(())
    }
}

pub(crate) fn boottime_now_ns() -> u64 {
    unsafe {
        let mut time: libc::timespec = zeroed();
        if libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) != 0 {
            return 0;
        }
        (time.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(time.tv_nsec as u64)
    }
}

pub fn validate_scan_root(scan_root: &File, expected: SnapshotIdentity) -> Result<(), ScanError> {
    if !scan_root
        .metadata()
        .map_err(|err| {
            ScanError::new(
                ScanErrorKind::MalformedResponse,
                format!("inspect AWACS scan root metadata: {err}"),
            )
        })?
        .is_dir()
    {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan root fd is not a directory",
        ));
    }
    let filesystem = crate::btrfs::filesystem_info(scan_root.as_fd()).map_err(|err| {
        ScanError::new(
            ScanErrorKind::MalformedResponse,
            format!("inspect AWACS scan root filesystem: {err}"),
        )
    })?;
    let subvolume = crate::btrfs::subvolume_info(scan_root.as_fd()).map_err(|err| {
        ScanError::new(
            ScanErrorKind::MalformedResponse,
            format!("inspect AWACS scan root subvolume: {err}"),
        )
    })?;
    if filesystem.fs_uuid != expected.filesystem_uuid
        || subvolume.uuid != expected.subvolume_uuid
        || !expected.read_only
        || !subvolume.readonly()
    {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan root identity or read-only flag does not match its response",
        ));
    }
    Ok(())
}

pub trait ScanClient: Send {
    fn begin_scan(&mut self, request: &BeginScanRequest) -> Result<SnapshotLease, ScanError>;
    fn release_baseline(&mut self, baseline_owner_id: [u8; 16]) -> Result<(), ScanError>;

    fn validate_scan_root(&self, lease: &SnapshotLease) -> Result<(), ScanError> {
        validate_scan_root(lease.scan_root(), lease.next_baseline.identity)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn baseline(token: &[u8]) -> SnapshotBaseline {
        SnapshotBaseline {
            identity: SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: token.to_vec(),
            retention_token: Vec::new(),
        }
    }

    struct RecordingSession {
        renewals: Arc<Mutex<usize>>,
        outcomes: Arc<Mutex<Vec<ScanOutcome>>>,
    }

    impl ScanSession for RecordingSession {
        fn renew(&mut self) -> Result<(), ScanError> {
            *self.renewals.lock().unwrap() += 1;
            Ok(())
        }

        fn promote(&mut self) -> Result<(), ScanError> {
            Ok(())
        }

        fn finish(&mut self, outcome: ScanOutcome) -> Result<(), ScanError> {
            self.outcomes.lock().unwrap().push(outcome);
            Ok(())
        }
    }

    #[test]
    fn lease_keeps_root_and_forwards_lifecycle() {
        let temp_dir = tempfile::tempdir().unwrap();
        let scan_root = File::open(temp_dir.path()).unwrap();
        let renewals = Arc::new(Mutex::new(0));
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let session = RecordingSession {
            renewals: renewals.clone(),
            outcomes: outcomes.clone(),
        };
        let mut lease = SnapshotLease::new(
            baseline(b"baseline"),
            Invalidation::Full,
            300,
            scan_root,
            Box::new(session),
        );
        assert!(lease.scan_root().metadata().unwrap().is_dir());
        lease.renew().unwrap();
        lease.finish(ScanOutcome::Committed).unwrap();
        lease.finish(ScanOutcome::Committed).unwrap();
        assert!(lease.finish(ScanOutcome::Aborted).is_err());
        assert_eq!(*renewals.lock().unwrap(), 1);
        assert_eq!(
            outcomes.lock().unwrap().as_slice(),
            &[ScanOutcome::Committed]
        );
    }

    #[test]
    fn error_retains_stable_classification() {
        let error = ScanError::new(ScanErrorKind::LeaseExpired, "lease expired");
        assert_eq!(error.kind(), ScanErrorKind::LeaseExpired);
        assert_eq!(error.message(), "lease expired");
        assert_eq!(error.to_string(), "lease expired");
    }

    #[test]
    fn scan_root_validation_rejects_unmatched_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let scan_root = File::open(temp_dir.path()).unwrap();
        let error = validate_scan_root(
            &scan_root,
            SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ScanErrorKind::MalformedResponse);
    }
}
