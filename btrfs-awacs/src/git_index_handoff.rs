//! Transfer a frozen Git index across snapshot watches without depending on
//! the physical lifetime of its persisted fsmonitor cursor's snapshot.

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::io::{self};
use std::ops::Range;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use crate::git_index_invalidation::invalidate_fsmonitor_bitmap;
use crate::recursive_snapshot::child_subvolume_paths;
use crate::scan::BeginScanRequest;
use crate::scan::Invalidation;
use crate::scan::ScanClient;
use crate::scan::ScanErrorKind;
use crate::scan::ScanOutcome;
use crate::scan::SnapshotBaseline;
use crate::scan::SnapshotLease;
use crate::scan_facade::DirectScanClient;
use crate::status::decode_git_fsmonitor_token;
use crate::status::encode_git_fsmonitor_token;
use crate::status::git_index_checksum_slices;
#[cfg(test)]
use crate::status::git_index_extension_data_range;
use crate::status::git_index_layout;
use crate::status::read_be_u32;
#[cfg(test)]
use crate::status::read_git_index_extension;
use crate::status::read_git_index_extension_from_file;

#[cfg(test)]
fn inherited_git_fsmonitor_baseline(
    source_index: &Path,
    hash_len: usize,
) -> Result<SnapshotBaseline, String> {
    let fsmonitor = read_git_index_extension(source_index, hash_len, b"FSMN")?
        .ok_or_else(|| "source Git index has no fsmonitor cache extension".to_owned())?;
    git_fsmonitor_baseline(&fsmonitor)
}

fn git_fsmonitor_baseline(fsmonitor: &[u8]) -> Result<SnapshotBaseline, String> {
    if fsmonitor.len() < 5 || read_be_u32(&fsmonitor)? != 2 {
        return Err("source Git fsmonitor cache is not protocol v2".to_owned());
    }
    let token_end = fsmonitor[4..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| 4 + offset)
        .ok_or_else(|| "source Git fsmonitor token is not NUL terminated".to_owned())?;
    decode_git_fsmonitor_token(&fsmonitor[4..token_end])
        .ok_or_else(|| "source Git fsmonitor token is not an AWACS token".to_owned())
}

/// A Git cursor is a logical replay position, not a physical snapshot pin.
/// Keep an immutable copy of the index and a newly leased cut together until
/// the child has reconciled both the replay and the source-cut -> child-cut
/// transition.
pub struct GitIndexHandoff {
    index: File,
    permissions: fs::Permissions,
    lease: Option<SnapshotLease>,
    nested_paths: Vec<Vec<u8>>,
}

fn capture_index_error(phase: &'static str, error: io::Error) -> String {
    // The optimization may fall back to a cold index. Report enough to
    // distinguish unsupported filesystems, permissions, and I/O failures
    // without logging a worktree path or Git index contents.
    tracing::warn!(
        phase,
        error_kind = ?error.kind(),
        os_error = error.raw_os_error(),
        "source Git index capture failed"
    );
    format!("{phase}: {error}")
}

impl GitIndexHandoff {
    pub fn capture(
        source: &Path,
        index_path: &Path,
        source_index: File,
        hash_len: usize,
    ) -> Result<Self, String> {
        // Capture the file before reading its cursor. An anonymous reflink
        // survives both atomic index replacement and later in-place writes,
        // without leaving a temporary file in the source checkout.
        let mut index = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_TMPFILE)
            .mode(0o600)
            .open(index_path.parent().ok_or("source index has no parent")?)
            .map_err(|error| capture_index_error("open temporary index", error))?;
        clone_or_copy_index(&source_index, &mut index)
            .map_err(|error| capture_index_error("copy source index", error))?;
        let permissions = source_index
            .metadata()
            .map_err(|error| capture_index_error("inspect index permissions", error))?
            .permissions();
        let root = File::open(source)
            .map_err(|error| capture_index_error("open source subvolume", error))?;
        let nested_paths = child_subvolume_paths(root.as_fd())
            .map_err(|error| capture_index_error("inspect nested subvolumes", error))?
            .into_iter()
            .map(|path| path.into_os_string().into_vec())
            .collect();
        let source_lease = (|| {
            let fsmonitor = read_git_index_extension_from_file(&mut index, hash_len, b"FSMN")?
                .ok_or_else(|| "source Git index has no fsmonitor cache extension".to_owned())?;
            let baseline = git_fsmonitor_baseline(&fsmonitor)?;
            let owner_id = baseline
                .retention_token
                .as_slice()
                .try_into()
                .map_err(|_| "source Git cursor has an invalid lane owner".to_owned())?;
            let mut client =
                DirectScanClient::for_root(source).map_err(|error| error.to_string())?;
            client
                .begin_scan(&BeginScanRequest {
                    live_root: source.to_owned(),
                    baseline_owner_id: owner_id,
                    previous_baseline: Some(baseline),
                    // An expired or absent replay proof requires all copied
                    // cache claims to be invalidated, not a fabricated baseline.
                    allow_full_invalidation: true,
                })
                .map_err(|error| error.to_string())
        })();
        let lease = match source_lease {
            Ok(lease) => Some(lease),
            Err(error) => {
                tracing::warn!(%error, "source Git replay unavailable; invalidating copied caches");
                None
            }
        };
        Ok(Self {
            index,
            permissions,
            lease,
            nested_paths,
        })
    }

    pub fn copy_index(&mut self, destination: &Path) -> Result<(), String> {
        let result = (|| -> io::Result<()> {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            clone_or_copy_index(&self.index, &mut output)?;
            output.set_permissions(self.permissions.clone())?;
            Ok(())
        })();
        result.map_err(|error| format!("copy captured source Git index: {error}"))
    }

    /// Reconciles the captured cursor through its leased cut to the child's
    /// cut. Missing source proof invalidates caches rather than requiring
    /// an old cursor's physical snapshot to survive collection. Both leases
    /// remain held until the caller publishes the reconciled child index.
    pub fn reconcile(
        &mut self,
        git_client: &mut DirectScanClient,
        git_lease: &mut SnapshotLease,
        destination: &Path,
    ) -> Result<Invalidation, String> {
        let Some(source_lease) = &mut self.lease else {
            return Ok::<_, String>(Invalidation::Full);
        };
        if matches!(source_lease.invalidation, Invalidation::Full) {
            return Ok(Invalidation::Full);
        }
        let bridge = match git_client.compare_scan_roots(source_lease, git_lease) {
            Ok(bridge) => bridge,
            Err(error)
                if matches!(
                    error.kind(),
                    ScanErrorKind::LeaseExpired
                        | ScanErrorKind::FullScanRequired
                        | ScanErrorKind::InvalidPreviousBaseline
                ) =>
            {
                // Losing source proof only loses the warm-cache
                // optimization. The child's own cut must still be valid.
                git_lease.renew().map_err(|error| error.to_string())?;
                git_client
                    .validate_scan_root(git_lease)
                    .map_err(|error| error.to_string())?;
                tracing::warn!(%error, "source comparison proof unavailable; invalidating copied caches");
                return Ok(Invalidation::Full);
            }
            Err(error) => {
                return Err(format!("reconcile source and child snapshots: {error}"));
            }
        };
        let child_root =
            File::open(destination).map_err(|error| format!("open child subvolume: {error}"))?;
        let child_paths = child_subvolume_paths(child_root.as_fd())
            .map_err(|error| format!("inspect child nested subvolumes: {error}"))?
            .into_iter()
            .map(|path| path.into_os_string().into_vec())
            .collect();
        Ok(merge_git_invalidations([
            source_lease.invalidation.clone(),
            bridge,
            Invalidation::Prefixes(self.nested_paths.clone()),
            Invalidation::Prefixes(child_paths),
        ]))
    }
}

impl Drop for GitIndexHandoff {
    fn drop(&mut self) {
        if let Some(lease) = &mut self.lease {
            // Creation never publishes a new source index. Only the temporary
            // physical pin is being released, on success and on rollback.
            if let Err(error) = lease.finish(ScanOutcome::Aborted) {
                tracing::warn!(%error, "release source Git creation lease");
            }
        }
    }
}

fn merge_git_invalidations(invalidations: impl IntoIterator<Item = Invalidation>) -> Invalidation {
    let mut paths = Vec::new();
    for invalidation in invalidations {
        match invalidation {
            Invalidation::Full => return Invalidation::Full,
            Invalidation::ExactPaths(mut changed) | Invalidation::Prefixes(mut changed) => {
                paths.append(&mut changed);
            }
        }
    }
    // Git never indexes its own administrative directory. Keep .jj events:
    // a Git-only repository is allowed to track a directory with that name.
    paths.retain(|path| !matches!(path.split(|byte| *byte == b'/').next(), Some(b".git")));
    paths.sort_unstable();
    paths.dedup();
    Invalidation::Prefixes(paths)
}

/// Records enclosing wall time, including failures, without logging command
/// arguments, paths, or fsmonitor cursors. Nested phases are identified by the
/// tracing span stack and must not be added to their parent's elapsed time.
pub fn timed_git_phase<T, E>(
    phase: &'static str,
    action: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let _span = tracing::info_span!("git_phase", phase, pid = std::process::id()).entered();
    let started = Instant::now();
    tracing::debug!(phase, "AWACS Git phase started");
    let result = action();
    tracing::info!(
        phase,
        pid = std::process::id(),
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        success = result.is_ok(),
        "AWACS Git phase completed"
    );
    result
}

#[cfg(test)]
fn copy_inherited_git_index(source_index: &Path, destination_index: &Path) -> Result<(), String> {
    fs::copy(source_index, destination_index).map_err(|error| {
        format!(
            "write inherited Git index {}: {error}",
            destination_index.display()
        )
    })?;
    Ok(())
}

/// Writes destination-owned fsmonitor and untracked-cache index extensions.
///
/// The copied source token cannot cross worktrees because its retention token
/// owns the source watch's Git lane. Preserve already dirty entries and add
/// all changes from its cursor through the destination cut before replacing
/// the cursor. Without a source extension or replay proof, mark all entries
/// dirty.
///
/// Git also associates its untracked cache with an absolute worktree path.
/// Relocate its identifier only for an empty transition; otherwise discard
/// its validity claims and let Git rebuild it against the actual destination.
pub fn seed_git_fsmonitor_index(
    index_path: &Path,
    worktree: &Path,
    hash_len: usize,
    baseline: &SnapshotBaseline,
    invalidation: &Invalidation,
) -> Result<(), String> {
    // Keep this exact file open through publication. Cloning a path reopened
    // after parsing could combine another writer's entries with our checksum.
    let (source, source_metadata, bytes) = timed_git_phase("index_seed_read", || {
        let mut source = File::open(index_path)
            .map_err(|error| format!("open destination Git index: {error}"))?;
        let source_metadata = source
            .metadata()
            .map_err(|error| format!("inspect destination Git index: {error}"))?;
        let capacity = usize::try_from(source_metadata.len())
            .map_err(|_| "destination Git index is too large".to_owned())?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|error| format!("allocate destination Git index: {error}"))?;
        source
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read destination Git index: {error}"))?;
        Ok::<_, String>((source, source_metadata, bytes))
    })?;
    let layout = timed_git_phase("index_seed_layout", || git_index_layout(&bytes, hash_len))?;
    let checksum_start = bytes.len() - hash_len;
    let fsmonitor = timed_git_phase("index_seed_find_fsmonitor", || {
        Ok::<_, String>(layout.extension_data_range(b"FSMN"))
    })?;
    let untracked = timed_git_phase("index_seed_find_untracked", || {
        Ok::<_, String>(layout.extension_data_range(b"UNTR"))
    })?;
    let token = encode_git_fsmonitor_token(baseline)?;
    let inherited_bitmap = match fsmonitor
        .as_ref()
        .filter(|_| !matches!(invalidation, Invalidation::Full))
    {
        Some(range) => {
            let existing = &bytes[range.clone()];
            if existing.len() < 5 || read_be_u32(&existing[..4])? != 2 {
                return Err("inherited Git fsmonitor cache is not protocol v2".to_owned());
            }
            let token_end = existing[4..]
                .iter()
                .position(|byte| *byte == 0)
                .map(|offset| offset + 4)
                .ok_or_else(|| "inherited Git fsmonitor token is not NUL terminated".to_owned())?;
            let bitmap = &existing[token_end + 1..];
            if bitmap.len() < 4 {
                return Err("inherited Git fsmonitor dirty bitmap has no length".to_owned());
            }
            let bitmap_len = usize::try_from(read_be_u32(bitmap)?)
                .map_err(|_| "inherited Git fsmonitor dirty bitmap is too large".to_owned())?;
            if bitmap.len() != bitmap_len + 4 {
                return Err("inherited Git fsmonitor dirty bitmap has an invalid length".to_owned());
            }
            Some(bitmap)
        }
        None => None,
    };
    let unchanged = match invalidation {
        Invalidation::ExactPaths(paths) | Invalidation::Prefixes(paths) => paths.is_empty(),
        Invalidation::Full => false,
    };
    let dirty_bitmap = inherited_bitmap
        .map(|bitmap| {
            if unchanged {
                Ok(bitmap.to_vec())
            } else {
                invalidate_fsmonitor_bitmap(&bytes, hash_len, bitmap, invalidation)
            }
        })
        .transpose()?;
    let fsmonitor_payload =
        git_fsmonitor_extension_payload(&token, dirty_bitmap.as_deref(), layout.entries);
    let mut replacements = vec![GitIndexExtensionReplacement::new(
        fsmonitor,
        b"FSMN",
        fsmonitor_payload,
        &[],
    )?];
    if let Some(range) = untracked {
        if unchanged && inherited_bitmap.is_some() {
            let (prefix, suffix) = relocate_git_untracked_cache(&bytes[range.clone()], worktree)?;
            replacements.push(GitIndexExtensionReplacement::new(
                Some(range),
                b"UNTR",
                prefix,
                suffix,
            )?);
        } else {
            // UNTR embeds validity claims for whole directories. Rebuild it
            // through Git when any path changed; keeping its cached contents
            // while advancing FSMN would hide newly copied untracked files.
            let mut replacement =
                GitIndexExtensionReplacement::new(Some(range), b"UNTR", Vec::new(), &[])?;
            replacement.header = Vec::new();
            replacements.push(replacement);
        }
    }
    replacements.sort_unstable_by_key(|replacement| replacement.header_start(checksum_start));
    let unchanged_prefix_len = replacements[0].header_start(checksum_start);
    let mut slices = Vec::with_capacity(4 * replacements.len() + 2);
    let mut offset = 0;
    for replacement in &replacements {
        let header_start = replacement.header_start(checksum_start);
        slices.push(&bytes[offset..header_start]);
        slices.push(replacement.header.as_slice());
        slices.push(replacement.prefix.as_slice());
        slices.push(replacement.suffix);
        offset = replacement
            .range
            .as_ref()
            .map_or(checksum_start, |range| range.end);
    }
    slices.push(&bytes[offset..checksum_start]);
    let checksum = timed_git_phase("index_seed_checksum", || {
        git_index_checksum_slices(slices.iter().copied(), hash_len)
    })?;
    slices.push(&checksum);
    write_git_index_atomically(
        index_path,
        &source,
        &source_metadata,
        &slices,
        unchanged_prefix_len,
    )
}

struct GitIndexExtensionReplacement<'a> {
    range: Option<Range<usize>>,
    header: Vec<u8>,
    prefix: Vec<u8>,
    suffix: &'a [u8],
}

impl<'a> GitIndexExtensionReplacement<'a> {
    fn new(
        range: Option<Range<usize>>,
        signature: &[u8; 4],
        prefix: Vec<u8>,
        suffix: &'a [u8],
    ) -> Result<Self, String> {
        let size = prefix
            .len()
            .checked_add(suffix.len())
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| "Git index extension is too large".to_owned())?;
        if range.as_ref().is_some_and(|range| range.start < 8) {
            return Err("Git index extension header is malformed".to_owned());
        }
        let mut header = vec![0; 8];
        header[..4].copy_from_slice(signature);
        header[4..].copy_from_slice(&size.to_be_bytes());
        Ok(Self {
            range,
            header,
            prefix,
            suffix,
        })
    }

    fn header_start(&self, checksum_start: usize) -> usize {
        self.range
            .as_ref()
            .map_or(checksum_start, |range| range.start - 8)
    }
}

fn relocate_git_untracked_cache<'a>(
    payload: &'a [u8],
    worktree: &Path,
) -> Result<(Vec<u8>, &'a [u8]), String> {
    let (identity_len, identity_start) = read_git_varint(payload)?;
    let identity_end = identity_start
        .checked_add(identity_len)
        .filter(|end| *end <= payload.len())
        .ok_or_else(|| "inherited Git untracked cache has a truncated identifier".to_owned())?;
    let identity = &payload[identity_start..identity_end];
    let first_identity_end = identity
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(identity.len());
    let first_identity = &identity[..first_identity_end];
    let location_prefix = b"Location ";
    let system_marker = b", system ";
    if !first_identity.starts_with(location_prefix) {
        return Err("inherited Git untracked cache has an invalid location identifier".to_owned());
    }
    let system_start = first_identity
        .windows(system_marker.len())
        .rposition(|window| window == system_marker)
        .filter(|offset| *offset >= location_prefix.len())
        .ok_or_else(|| "inherited Git untracked cache has no system identifier".to_owned())?;
    if system_start + system_marker.len() == first_identity.len() {
        return Err("inherited Git untracked cache has an empty system identifier".to_owned());
    }

    let mut relocated_identity = Vec::with_capacity(identity.len() + worktree.as_os_str().len());
    relocated_identity.extend_from_slice(location_prefix);
    relocated_identity.extend_from_slice(worktree.as_os_str().as_bytes());
    relocated_identity.extend_from_slice(&first_identity[system_start..]);
    relocated_identity.extend_from_slice(&identity[first_identity_end..]);

    // The cached directory payload is unchanged; borrow it instead of copying
    // the entire UNTR extension to replace its small location identifier.
    let mut prefix = Vec::with_capacity(16 + relocated_identity.len());
    append_git_varint(&mut prefix, relocated_identity.len());
    prefix.extend_from_slice(&relocated_identity);
    Ok((prefix, &payload[identity_end..]))
}

fn read_git_varint(bytes: &[u8]) -> Result<(usize, usize), String> {
    let mut offset = 0;
    let mut byte = *bytes.get(offset).ok_or_else(|| {
        "inherited Git untracked cache has a truncated identifier length".to_owned()
    })?;
    offset += 1;
    let mut value = usize::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = *bytes.get(offset).ok_or_else(|| {
            "inherited Git untracked cache has a truncated identifier length".to_owned()
        })?;
        offset += 1;
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_mul(128))
            .and_then(|value| value.checked_add(usize::from(byte & 0x7f)))
            .ok_or_else(|| "inherited Git untracked cache identifier is too large".to_owned())?;
    }
    Ok((value, offset))
}

fn append_git_varint(bytes: &mut Vec<u8>, mut value: usize) {
    let mut encoded = [0_u8; 16];
    let mut offset = encoded.len() - 1;
    encoded[offset] = (value & 0x7f) as u8;
    while {
        value >>= 7;
        value != 0
    } {
        value -= 1;
        offset -= 1;
        encoded[offset] = 0x80 | (value & 0x7f) as u8;
    }
    bytes.extend_from_slice(&encoded[offset..]);
}

fn git_fsmonitor_extension_payload(
    token: &str,
    inherited_bitmap: Option<&[u8]>,
    index_entries: u32,
) -> Vec<u8> {
    let mut payload = 2u32.to_be_bytes().to_vec();
    payload.extend_from_slice(token.as_bytes());
    payload.push(0);
    match inherited_bitmap {
        Some(bitmap) => payload.extend_from_slice(bitmap),
        None => {
            // Without a source FSMN extension, no entry can safely inherit a
            // clean bit. Encode an all-dirty EWAH as a run of full one words
            // followed by one partial literal when needed.
            let full_words = index_entries / 64;
            let partial_bits = index_entries % 64;
            let literal_words = u32::from(partial_bits != 0);
            let bitmap_len = 20 + literal_words * 8;
            payload.extend_from_slice(&bitmap_len.to_be_bytes());
            payload.extend_from_slice(&index_entries.to_be_bytes());
            payload.extend_from_slice(&(1 + literal_words).to_be_bytes());
            let run_bit = u64::from(full_words != 0);
            let run_length_word =
                run_bit | (u64::from(full_words) << 1) | (u64::from(literal_words) << 33);
            payload.extend_from_slice(&run_length_word.to_be_bytes());
            if partial_bits != 0 {
                let dirty_bits = (1_u64 << partial_bits) - 1;
                payload.extend_from_slice(&dirty_bits.to_be_bytes());
            }
            payload.extend_from_slice(&0_u32.to_be_bytes());
        }
    }
    payload
}

#[cfg(test)]
fn append_git_index_extension(
    bytes: &mut Vec<u8>,
    signature: &[u8; 4],
    payload: &[u8],
) -> Result<(), String> {
    let size =
        u32::try_from(payload.len()).map_err(|_| "Git index extension is too large".to_owned())?;
    bytes.extend_from_slice(signature);
    bytes.extend_from_slice(&size.to_be_bytes());
    bytes.extend_from_slice(payload);
    Ok(())
}

fn write_git_index_atomically(
    index_path: &Path,
    source: &File,
    source_metadata: &fs::Metadata,
    slices: &[&[u8]],
    unchanged_prefix_len: usize,
) -> Result<(), String> {
    write_git_index_with_clone(
        index_path,
        source,
        source_metadata,
        slices,
        unchanged_prefix_len,
        try_clone_git_index,
    )
}

/// Reflinks an entire index when supported, otherwise copies its bytes.
/// Both files start at offset zero and the destination's old contents are
/// replaced. The source inode remains unchanged.
pub fn clone_or_copy_index(source: &File, destination: &mut File) -> io::Result<()> {
    if !try_clone_git_index(source, destination)? {
        let mut source = source;
        source.rewind()?;
        destination.set_len(0)?;
        destination.rewind()?;
        io::copy(&mut source, destination)?;
    }
    Ok(())
}

fn try_clone_git_index(source: &File, destination: &File) -> io::Result<bool> {
    // SAFETY: both descriptors remain live, destination is writable, and
    // FICLONE takes the source descriptor directly rather than a pointer.
    if unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EXDEV | libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL | libc::ENOSYS) => {
            Ok(false)
        }
        _ => Err(error),
    }
}

fn git_index_metadata_matches(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn write_git_index_with_clone(
    index_path: &Path,
    source: &File,
    source_metadata: &fs::Metadata,
    slices: &[&[u8]],
    unchanged_prefix_len: usize,
    clone_index: impl FnOnce(&File, &File) -> io::Result<bool>,
) -> Result<(), String> {
    let mut lock_path = index_path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let mut lock = timed_git_phase("index_seed_lock", || {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
    })
    .map_err(|error| {
        format!(
            "lock destination Git index {}: {error}",
            index_path.display()
        )
    })?;
    let result = (|| {
        let current_metadata = fs::metadata(index_path)
            .map_err(|error| format!("recheck destination Git index: {error}"))?;
        let opened_metadata = source
            .metadata()
            .map_err(|error| format!("recheck opened Git index: {error}"))?;
        if !git_index_metadata_matches(source_metadata, &current_metadata)
            || !git_index_metadata_matches(source_metadata, &opened_metadata)
        {
            return Err("destination Git index changed while preparing its extensions".to_owned());
        }
        let new_len = slices.iter().try_fold(0_u64, |length, bytes| {
            length
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| "rewritten Git index is too large".to_owned())
        })?;
        if unchanged_prefix_len as u64 > source_metadata.len()
            || unchanged_prefix_len as u64 > new_len
        {
            return Err(
                "unchanged Git index prefix exceeds the source or rewritten index".to_owned(),
            );
        }
        let cloned = timed_git_phase("index_seed_clone", || clone_index(source, &lock))
            .map_err(|error| format!("clone destination Git index: {error}"))?;
        timed_git_phase("index_seed_write", || {
            let mut skip = if cloned { unchanged_prefix_len } else { 0 };
            if cloned {
                lock.seek(SeekFrom::Start(skip as u64))
                    .map_err(|error| format!("seek destination Git index: {error}"))?;
            } else {
                // An unsupported clone attempt must not leave any partial data.
                lock.set_len(0)
                    .and_then(|()| lock.rewind())
                    .map_err(|error| format!("reset destination Git index: {error}"))?;
            }
            for bytes in slices {
                let skipped = skip.min(bytes.len());
                skip -= skipped;
                lock.write_all(&bytes[skipped..])
                    .map_err(|error| format!("write destination Git index: {error}"))?;
            }
            if skip != 0 {
                return Err("unchanged Git index prefix exceeds rewritten index".to_owned());
            }
            lock.set_len(new_len)
                .map_err(|error| format!("resize destination Git index: {error}"))
        })?;
        timed_git_phase("index_seed_sync", || lock.sync_all()).map_err(|error| {
            format!(
                "sync destination Git index {}: {error}",
                index_path.display()
            )
        })?;
        timed_git_phase("index_seed_publish", || fs::rename(&lock_path, index_path)).map_err(
            |error| {
                format!(
                    "commit destination Git index {}: {error}",
                    index_path.display()
                )
            },
        )?;
        tracing::debug!(
            cloned,
            unchanged_prefix_len,
            new_len,
            written_bytes = new_len
                - if cloned {
                    unchanged_prefix_len as u64
                } else {
                    0
                },
            "seeded Git index committed"
        );
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&lock_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use sha1::Digest as _;

    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "btrfs-awacs-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn git_index_path(cwd: &Path) -> Result<PathBuf, String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "--git-path", "index"])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        let value = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
        let path = PathBuf::from(value.trim_end());
        Ok(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        })
    }

    // Contiguous-buffer algorithm, retained as an independent wire format
    // oracle for the fragment/reflink writer below.
    fn reference_seeded_index(
        bytes: &[u8],
        worktree: &Path,
        hash_len: usize,
        baseline: &crate::scan::SnapshotBaseline,
    ) -> Result<Vec<u8>, String> {
        let checksum_start = bytes
            .len()
            .checked_sub(hash_len)
            .ok_or_else(|| "Git index has no checksum trailer".to_owned())?;
        let fsmonitor = super::git_index_extension_data_range(&bytes, hash_len, b"FSMN")?;
        let untracked = super::git_index_extension_data_range(&bytes, hash_len, b"UNTR")?;
        let token = super::encode_git_fsmonitor_token(baseline)?;
        let inherited_bitmap = match fsmonitor.as_ref() {
            Some(range) => {
                let existing = &bytes[range.clone()];
                if existing.len() < 5 || super::read_be_u32(&existing[..4])? != 2 {
                    return Err("inherited Git fsmonitor cache is not protocol v2".to_owned());
                }
                let token_end = existing[4..]
                    .iter()
                    .position(|byte| *byte == 0)
                    .map(|offset| offset + 4)
                    .ok_or_else(|| {
                        "inherited Git fsmonitor token is not NUL terminated".to_owned()
                    })?;
                let bitmap = &existing[token_end + 1..];
                if bitmap.len() < 4 {
                    return Err("inherited Git fsmonitor dirty bitmap has no length".to_owned());
                }
                let bitmap_len = usize::try_from(super::read_be_u32(bitmap)?)
                    .map_err(|_| "inherited Git fsmonitor dirty bitmap is too large".to_owned())?;
                if bitmap.len() != bitmap_len + 4 {
                    return Err(
                        "inherited Git fsmonitor dirty bitmap has an invalid length".to_owned()
                    );
                }
                Some(bitmap)
            }
            None => None,
        };
        let index_entries = super::read_be_u32(&bytes[8..12])?;
        let fsmonitor_payload =
            super::git_fsmonitor_extension_payload(&token, inherited_bitmap, index_entries);
        let untracked_payload = untracked
            .as_ref()
            .map(|range| reference_relocate_git_untracked_cache(&bytes[range.clone()], worktree))
            .transpose()?;

        let mut replacements = Vec::with_capacity(2);
        if let Some(range) = fsmonitor.as_ref() {
            replacements.push((range, b"FSMN", fsmonitor_payload.as_slice()));
        }
        if let (Some(range), Some(payload)) = (untracked.as_ref(), untracked_payload.as_ref()) {
            replacements.push((range, b"UNTR", payload.as_slice()));
        }
        replacements.sort_unstable_by_key(|(range, _, _)| range.start);

        let mut rewritten = Vec::with_capacity(bytes.len() + fsmonitor_payload.len() + 8);
        let mut offset = 0;
        for (range, signature, payload) in replacements {
            let header_start = range
                .start
                .checked_sub(8)
                .ok_or_else(|| "Git index extension header is malformed".to_owned())?;
            rewritten.extend_from_slice(&bytes[offset..header_start]);
            if signature != b"UNTR" || inherited_bitmap.is_some() {
                super::append_git_index_extension(&mut rewritten, signature, payload)?;
            }
            offset = range.end;
        }
        rewritten.extend_from_slice(&bytes[offset..checksum_start]);
        if fsmonitor.is_none() {
            super::append_git_index_extension(&mut rewritten, b"FSMN", &fsmonitor_payload)?;
        }
        let checksum = crate::status::git_index_checksum(&rewritten, hash_len)?;
        rewritten.extend_from_slice(&checksum);
        Ok(rewritten)
    }

    fn reference_relocate_git_untracked_cache(
        payload: &[u8],
        worktree: &Path,
    ) -> Result<Vec<u8>, String> {
        let (identity_len, identity_start) = super::read_git_varint(payload)?;
        let identity_end = identity_start
            .checked_add(identity_len)
            .filter(|end| *end <= payload.len())
            .ok_or_else(|| "inherited Git untracked cache has a truncated identifier".to_owned())?;
        let identity = &payload[identity_start..identity_end];
        let first_identity_end = identity
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(identity.len());
        let first_identity = &identity[..first_identity_end];
        let location_prefix = b"Location ";
        let system_marker = b", system ";
        if !first_identity.starts_with(location_prefix) {
            return Err(
                "inherited Git untracked cache has an invalid location identifier".to_owned(),
            );
        }
        let system_start = first_identity
            .windows(system_marker.len())
            .rposition(|window| window == system_marker)
            .filter(|offset| *offset >= location_prefix.len())
            .ok_or_else(|| "inherited Git untracked cache has no system identifier".to_owned())?;
        if system_start + system_marker.len() == first_identity.len() {
            return Err("inherited Git untracked cache has an empty system identifier".to_owned());
        }

        let mut relocated_identity =
            Vec::with_capacity(identity.len() + worktree.as_os_str().len());
        relocated_identity.extend_from_slice(location_prefix);
        relocated_identity.extend_from_slice(worktree.as_os_str().as_bytes());
        relocated_identity.extend_from_slice(&first_identity[system_start..]);
        relocated_identity.extend_from_slice(&identity[first_identity_end..]);

        let mut relocated = Vec::with_capacity(payload.len() + relocated_identity.len());
        super::append_git_varint(&mut relocated, relocated_identity.len());
        relocated.extend_from_slice(&relocated_identity);
        relocated.extend_from_slice(&payload[identity_end..]);
        Ok(relocated)
    }

    #[test]
    fn streamed_index_matches_reference_bytes_for_extension_orders_and_hashes() {
        let baseline = crate::scan::SnapshotBaseline {
            identity: crate::scan::SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:1:destination".to_vec(),
            retention_token: vec![3; 16],
        };
        let old_token = super::encode_git_fsmonitor_token(&baseline).unwrap();
        let fsmonitor = super::git_fsmonitor_extension_payload(&old_token, None, 0);
        let mut untracked = Vec::new();
        let identity = b"Location /a/source/with/a/long/path, system Linux\0other identifier\0";
        super::append_git_varint(&mut untracked, identity.len());
        untracked.extend_from_slice(identity);
        untracked.extend_from_slice(&[0x5a; 8192]);
        let orders: &[&[&[u8; 4]]] = &[
            &[],
            &[b"FSMN"],
            &[b"UNTR"],
            &[b"FSMN", b"UNTR"],
            &[b"UNTR", b"FSMN"],
        ];
        for hash_len in [20, 32] {
            for order in orders {
                for destination in ["short".to_owned(), "long-destination/".repeat(16)] {
                    let root = TestDir::new("streamed-git-index");
                    let index = root.path.join("index");
                    let worktree = root.path.join(destination);
                    let mut bytes = b"DIRC".to_vec();
                    bytes.extend_from_slice(&2_u32.to_be_bytes());
                    bytes.extend_from_slice(&0_u32.to_be_bytes());
                    super::append_git_index_extension(&mut bytes, b"TREE", &[0x33; 8192]).unwrap();
                    for signature in *order {
                        let payload = if *signature == b"FSMN" {
                            &fsmonitor
                        } else {
                            &untracked
                        };
                        super::append_git_index_extension(&mut bytes, signature, payload).unwrap();
                        super::append_git_index_extension(&mut bytes, b"XTRA", b"unchanged")
                            .unwrap();
                    }
                    bytes.resize(bytes.len() + hash_len, 0);
                    let expected =
                        reference_seeded_index(&bytes, &worktree, hash_len, &baseline).unwrap();
                    fs::write(&index, &bytes).unwrap();
                    seed_git_fsmonitor_index(
                        &index,
                        &worktree,
                        hash_len,
                        &baseline,
                        &crate::scan::Invalidation::ExactPaths(Vec::new()),
                    )
                    .unwrap();
                    assert_eq!(
                        fs::read(&index).unwrap(),
                        expected,
                        "streamed output must match the complete serialized index"
                    );
                    assert!(!root.path.join("index.lock").exists());
                }
            }
        }
    }

    #[test]
    fn index_writer_handles_clone_and_partial_unsupported_clone_fallback() {
        use std::io::Seek as _;
        use std::io::Write as _;
        for cloned in [false, true] {
            for tail in [
                b"short".as_slice(),
                b"a much longer replacement tail".as_slice(),
            ] {
                let root = TestDir::new("index-writer-clone");
                let index = root.path.join("index");
                let prefix = [0x35; 8192];
                let mut original = prefix.to_vec();
                original.extend_from_slice(b"old tail data");
                fs::write(&index, &original).unwrap();
                let source = fs::File::open(&index).unwrap();
                let metadata = source.metadata().unwrap();
                let slices = [&prefix[..], &[][..], tail];
                super::write_git_index_with_clone(
                    &index,
                    &source,
                    &metadata,
                    &slices,
                    prefix.len(),
                    |source, destination| {
                        let mut destination = destination;
                        if cloned {
                            let mut source = source.try_clone()?;
                            source.rewind()?;
                            std::io::copy(&mut source, &mut destination)?;
                            Ok(true)
                        } else {
                            destination.write_all(b"partial unsupported clone")?;
                            Ok(false)
                        }
                    },
                )
                .unwrap();
                assert_eq!(fs::read(&index).unwrap(), slices.concat());
                assert!(!root.path.join("index.lock").exists());
                let mut source = source;
                source.rewind().unwrap();
                let mut retained = Vec::new();
                std::io::Read::read_to_end(&mut source, &mut retained).unwrap();
                assert_eq!(
                    retained, original,
                    "publication must not mutate the opened source"
                );
            }
        }
    }

    #[test]
    fn index_writer_preserves_existing_index_and_lock_on_failure() {
        let root = TestDir::new("index-writer-errors");
        let index = root.path.join("index");
        let lock = root.path.join("index.lock");
        fs::write(&index, b"original").unwrap();
        let source = fs::File::open(&index).unwrap();
        let metadata = source.metadata().unwrap();
        let slices = [b"replaced".as_slice()];
        fs::write(&lock, b"another writer").unwrap();
        assert!(
            super::write_git_index_with_clone(
                &index,
                &source,
                &metadata,
                &slices,
                0,
                |_, _| panic!("must not clone while another writer owns index.lock"),
            )
            .is_err()
        );
        assert_eq!(fs::read(&lock).unwrap(), b"another writer");
        fs::remove_file(&lock).unwrap();
        let error =
            super::write_git_index_with_clone(&index, &source, &metadata, &slices, 0, |_, _| {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            })
            .unwrap_err();
        assert!(error.contains("clone destination Git index"));
        assert_eq!(fs::read(&index).unwrap(), b"original");
        assert!(!lock.exists());

        let replacement = root.path.join("replacement");
        fs::write(&replacement, b"new writer's index").unwrap();
        fs::rename(&replacement, &index).unwrap();
        let error =
            super::write_git_index_with_clone(&index, &source, &metadata, &slices, 0, |_, _| {
                panic!("must not clone an index superseded by another writer")
            })
            .unwrap_err();
        assert!(error.contains("changed while preparing"));
        assert_eq!(fs::read(&index).unwrap(), b"new writer's index");
        assert!(!lock.exists());
    }

    #[test]
    fn git_handoff_keeps_changes_to_git_tracked_jj_paths() {
        use crate::scan::Invalidation;
        assert_eq!(
            super::merge_git_invalidations([
                Invalidation::ExactPaths(vec![b".git/index".to_vec(), b".jj/tracked".to_vec()]),
                Invalidation::Prefixes(vec![b"directory".to_vec()]),
            ]),
            Invalidation::Prefixes(vec![b".jj/tracked".to_vec(), b"directory".to_vec()]),
        );
    }

    #[test]
    fn copies_inherited_git_index_without_rewriting_fsmonitor() {
        let source_baseline = crate::scan::SnapshotBaseline {
            identity: crate::scan::SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:1:inherited".to_vec(),
            retention_token: vec![3; 16],
        };
        let source_token = encode_git_fsmonitor_token(&source_baseline).unwrap();
        let mut fsmonitor = 2u32.to_be_bytes().to_vec();
        fsmonitor.extend_from_slice(source_token.as_bytes());
        fsmonitor.push(0);
        fsmonitor.extend_from_slice(&0u32.to_be_bytes());

        let mut index = b"DIRC".to_vec();
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&0u32.to_be_bytes());
        index.extend_from_slice(b"FSMN");
        index.extend_from_slice(&(fsmonitor.len() as u32).to_be_bytes());
        index.extend_from_slice(&fsmonitor);
        index.extend_from_slice(b"UNTR");
        index.extend_from_slice(&3u32.to_be_bytes());
        index.extend_from_slice(b"old");
        index.extend_from_slice(&[0; 20]);

        let root = TestDir::new("inherited-git-index");
        let source_index = root.path.join("source-index");
        let destination_index = root.path.join("destination-index");
        fs::write(&source_index, &index).unwrap();
        let inherited = inherited_git_fsmonitor_baseline(&source_index, 20).unwrap();
        assert_eq!(inherited, source_baseline);
        copy_inherited_git_index(&source_index, &destination_index).unwrap();
        assert_eq!(
            fs::read(destination_index).unwrap(),
            index,
            "the destination index retains the source FSMN and UNTR bytes"
        );
    }

    #[test]
    fn seeds_destination_owned_fsmonitor_token_preserving_dirty_entries() {
        let source_baseline = crate::scan::SnapshotBaseline {
            identity: crate::scan::SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:1:source".to_vec(),
            retention_token: vec![3; 16],
        };
        let destination_baseline = crate::scan::SnapshotBaseline {
            identity: crate::scan::SnapshotIdentity {
                filesystem_uuid: [4; 16],
                subvolume_uuid: [5; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:1:destination".to_vec(),
            retention_token: vec![6; 16],
        };
        let source_token = encode_git_fsmonitor_token(&source_baseline).unwrap();
        let mut fsmonitor = 2u32.to_be_bytes().to_vec();
        fsmonitor.extend_from_slice(source_token.as_bytes());
        fsmonitor.push(0);
        let dirty_bitmap = [
            0, 0, 0, 3, // three indexed positions
            0, 0, 0, 2, // one run-length word and one literal word
            0, 0, 0, 2, 0, 0, 0, 0, // one literal word
            0, 0, 0, 0, 0, 0, 0, 5, // positions zero and two are dirty
            0, 0, 0, 0, // run-length word position
        ];
        fsmonitor.extend_from_slice(&(dirty_bitmap.len() as u32).to_be_bytes());
        fsmonitor.extend_from_slice(&dirty_bitmap);
        let source_untracked_identity = b"Location /source, system Linux";
        let cached_directories = b"warm cached directory data\0";
        let mut untracked = Vec::new();
        super::append_git_varint(&mut untracked, source_untracked_identity.len());
        untracked.extend_from_slice(source_untracked_identity);
        untracked.extend_from_slice(cached_directories);

        let mut index = b"DIRC".to_vec();
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&0u32.to_be_bytes());
        index.extend_from_slice(b"FSMN");
        index.extend_from_slice(&(fsmonitor.len() as u32).to_be_bytes());
        index.extend_from_slice(&fsmonitor);
        index.extend_from_slice(b"UNTR");
        index.extend_from_slice(&(untracked.len() as u32).to_be_bytes());
        index.extend_from_slice(&untracked);
        let mut extension_headers = sha1::Sha1::new();
        extension_headers.update(&index[12..20]);
        let untracked_header = 20 + fsmonitor.len();
        extension_headers.update(&index[untracked_header..untracked_header + 8]);
        let mut eoie = 12_u32.to_be_bytes().to_vec();
        eoie.extend_from_slice(&extension_headers.finalize());
        super::append_git_index_extension(&mut index, b"EOIE", &eoie).unwrap();
        index.extend_from_slice(&[0; 20]);

        let root = TestDir::new("seeded-git-index");
        let index_path = root.path.join("index");
        let destination = root.path.join("destination-".repeat(14));
        fs::write(&index_path, &index).unwrap();
        assert_eq!(
            inherited_git_fsmonitor_baseline(&index_path, 20).unwrap(),
            source_baseline,
            "the original token is readable through a valid EOIE"
        );
        seed_git_fsmonitor_index(
            &index_path,
            &destination,
            20,
            &destination_baseline,
            &crate::scan::Invalidation::ExactPaths(Vec::new()),
        )
        .unwrap();

        let rewritten = fs::read(&index_path).unwrap();
        // The seed changes extension lengths, so its inherited EOIE digest is
        // stale. The new file reader must still recover the destination token.
        let seeded = inherited_git_fsmonitor_baseline(&index_path, 20).unwrap();
        assert_eq!(seeded, destination_baseline);
        let untracked = super::git_index_extension_data_range(&rewritten, 20, b"UNTR")
            .unwrap()
            .expect("seeding must retain Git's untracked cache extension");
        let untracked = &rewritten[untracked];
        let (identity_len, identity_start) = super::read_git_varint(untracked).unwrap();
        let identity_end = identity_start + identity_len;
        assert_eq!(
            &untracked[identity_start..identity_end],
            format!("Location {}, system Linux", destination.display()).as_bytes(),
            "the copied untracked cache must name the destination worktree"
        );
        assert!(
            identity_len > 127,
            "the relocated identity should exercise Git's multibyte varint encoding"
        );
        assert_eq!(
            &untracked[identity_end..],
            cached_directories,
            "relocating the cache must preserve every cached directory byte"
        );
        let checksum_start = rewritten.len() - 20;
        assert_eq!(
            &rewritten[checksum_start..],
            sha1::Sha1::digest(&rewritten[..checksum_start]).as_slice(),
            "seeding must leave Git with a valid index checksum"
        );
        let fsmonitor = super::git_index_extension_data_range(&rewritten, 20, b"FSMN")
            .unwrap()
            .unwrap();
        let token_end = rewritten[fsmonitor.start + 4..fsmonitor.end]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap()
            + fsmonitor.start
            + 4;
        let ewah_size_start = token_end + 1;
        let ewah_size = super::read_be_u32(&rewritten[ewah_size_start..]).unwrap() as usize;
        assert_eq!(
            ewah_size,
            dirty_bitmap.len(),
            "the destination cursor must retain the source dirty bitmap size"
        );
        assert_eq!(
            &rewritten[ewah_size_start + 4..ewah_size_start + 4 + ewah_size],
            dirty_bitmap,
            "rewriting the destination cursor must preserve every dirty bitmap bit"
        );
    }

    #[test]
    fn seeds_empty_fsmonitor_extension_when_the_source_has_no_extension() {
        let destination_baseline = crate::scan::SnapshotBaseline {
            identity: crate::scan::SnapshotIdentity {
                filesystem_uuid: [4; 16],
                subvolume_uuid: [5; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:1:destination".to_vec(),
            retention_token: vec![6; 16],
        };
        let source_untracked_identity = b"Location /source, system Linux";
        let mut untracked = Vec::new();
        super::append_git_varint(&mut untracked, source_untracked_identity.len());
        untracked.extend_from_slice(source_untracked_identity);
        untracked.extend_from_slice(b"cached directory data\0");
        let mut index = b"DIRC".to_vec();
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&0u32.to_be_bytes());
        index.extend_from_slice(b"UNTR");
        index.extend_from_slice(&(untracked.len() as u32).to_be_bytes());
        index.extend_from_slice(&untracked);
        index.extend_from_slice(&[0; 20]);

        let root = TestDir::new("added-git-index");
        let index_path = root.path.join("index");
        fs::write(&index_path, &index).unwrap();
        seed_git_fsmonitor_index(
            &index_path,
            &root.path,
            20,
            &destination_baseline,
            &crate::scan::Invalidation::ExactPaths(Vec::new()),
        )
        .unwrap();
        assert_eq!(
            inherited_git_fsmonitor_baseline(&index_path, 20).unwrap(),
            destination_baseline
        );
    }

    #[test]
    fn git_untracked_cache_varints_round_trip_across_encoding_boundaries() {
        for value in [0, 1, 127, 128, 129, 16_383, 16_384, usize::MAX] {
            let mut encoded = Vec::new();
            super::append_git_varint(&mut encoded, value);
            assert_eq!(
                super::read_git_varint(&encoded).unwrap(),
                (value, encoded.len())
            );
        }
    }

    #[test]
    fn git_accepts_relocated_inherited_untracked_cache() {
        fn run_git(cwd: &Path, arguments: &[&str]) -> Vec<u8> {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        let root = TestDir::new("relocated-git-untracked-cache");
        let source = root.path.join("source");
        let destination = root.path.join("destination");
        fs::create_dir(&source).unwrap();
        run_git(&source, &["init"]);
        run_git(&source, &["config", "user.name", "Test User"]);
        run_git(&source, &["config", "user.email", "test@example.com"]);
        let hook = root.path.join("fsmonitor-hook");
        fs::write(&hook, b"#!/bin/sh\nprintf 'test-cursor\\0'\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        run_git(
            &source,
            &["config", "core.fsmonitor", hook.to_str().unwrap()],
        );
        run_git(&source, &["config", "core.fsmonitorHookVersion", "2"]);
        run_git(&source, &["config", "core.untrackedCache", "true"]);
        fs::write(source.join("tracked"), b"tracked\n").unwrap();
        run_git(&source, &["add", "tracked"]);
        run_git(&source, &["commit", "-m", "source"]);
        fs::create_dir(source.join("scratch")).unwrap();
        fs::write(source.join("scratch/untracked"), b"untracked\n").unwrap();
        assert_eq!(
            run_git(&source, &["status", "--porcelain"]),
            b"?? scratch/\n"
        );

        run_git(
            &source,
            &[
                "worktree",
                "add",
                "--no-checkout",
                "--detach",
                destination.to_str().unwrap(),
                "HEAD",
            ],
        );
        fs::copy(source.join("tracked"), destination.join("tracked")).unwrap();
        fs::create_dir(destination.join("scratch")).unwrap();
        fs::copy(
            source.join("scratch/untracked"),
            destination.join("scratch/untracked"),
        )
        .unwrap();

        let source_index = git_index_path(&source).unwrap();
        let destination_index = git_index_path(&destination).unwrap();
        let source_bytes = fs::read(&source_index).unwrap();
        let source_untracked = super::git_index_extension_data_range(&source_bytes, 20, b"UNTR")
            .unwrap()
            .expect("source Git status must populate a genuine untracked cache");
        let source_untracked = &source_bytes[source_untracked];
        let (source_identity_len, source_identity_start) =
            super::read_git_varint(source_untracked).unwrap();

        copy_inherited_git_index(&source_index, &destination_index).unwrap();
        let baseline = crate::scan::SnapshotBaseline {
            identity: crate::scan::SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:1:destination".to_vec(),
            retention_token: vec![3; 16],
        };
        seed_git_fsmonitor_index(
            &destination_index,
            &destination,
            20,
            &baseline,
            &crate::scan::Invalidation::ExactPaths(Vec::new()),
        )
        .unwrap();

        let destination_bytes = fs::read(&destination_index).unwrap();
        let destination_untracked =
            super::git_index_extension_data_range(&destination_bytes, 20, b"UNTR")
                .unwrap()
                .expect("destination index must retain the genuine untracked cache");
        let destination_untracked = &destination_bytes[destination_untracked];
        let (destination_identity_len, destination_identity_start) =
            super::read_git_varint(destination_untracked).unwrap();
        let destination_identity =
            &destination_untracked[destination_identity_start..][..destination_identity_len];
        let destination_primary_identity = destination_identity
            .split(|byte| *byte == 0)
            .next()
            .unwrap();
        assert_eq!(
            destination_primary_identity,
            format!("Location {}, system Linux", destination.display()).as_bytes()
        );
        assert_eq!(
            &destination_untracked[destination_identity_start + destination_identity_len..],
            &source_untracked[source_identity_start + source_identity_len..],
            "relocation must retain Git's complete serialized directory cache"
        );
        run_git(&destination, &["reset", "--hard", "--quiet", "HEAD"]);
        assert_eq!(
            run_git(&destination, &["status", "--porcelain"]),
            b"?? scratch/\n",
            "C Git must accept and reuse the relocated untracked-cache extension"
        );
    }

    #[test]
    fn fsmonitor_bitmap_without_source_extension_marks_every_entry_dirty() {
        for index_entries in [0, 1, 63, 64, 65, 130, 1_257_714_u32] {
            let payload = super::git_fsmonitor_extension_payload("cursor", None, index_entries);
            let bitmap = &payload[4 + "cursor".len() + 1..];
            let bitmap_len = super::read_be_u32(bitmap).unwrap() as usize;
            assert_eq!(bitmap.len(), bitmap_len + 4);
            assert_eq!(super::read_be_u32(&bitmap[4..]).unwrap(), index_entries);
            let compressed_words = super::read_be_u32(&bitmap[8..]).unwrap();
            let run_length_word = u64::from_be_bytes(bitmap[12..20].try_into().unwrap());
            let run_bit = run_length_word & 1;
            let full_words = (run_length_word >> 1) & u64::from(u32::MAX);
            let literal_words = run_length_word >> 33;
            assert_eq!(compressed_words, 1 + literal_words as u32);
            let mut dirty_entries = if run_bit == 0 { 0 } else { full_words * 64 };
            if literal_words != 0 {
                let literal = u64::from_be_bytes(bitmap[20..28].try_into().unwrap());
                dirty_entries += u64::from(literal.count_ones());
            }
            assert_eq!(
                dirty_entries,
                u64::from(index_entries),
                "every entry must be dirty when no source bitmap exists"
            );
        }
    }

    #[test]
    fn refuses_to_seed_fsmonitor_extension_from_initial_baseline() {
        let initial_baseline = crate::scan::SnapshotBaseline {
            identity: crate::scan::SnapshotIdentity {
                filesystem_uuid: [4; 16],
                subvolume_uuid: [5; 16],
                read_only: true,
            },
            continuity_token: Vec::new(),
            retention_token: vec![6; 16],
        };
        let mut index = b"DIRC".to_vec();
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&0u32.to_be_bytes());
        index.extend_from_slice(&[0; 20]);

        let root = TestDir::new("unverified-git-index");
        let index_path = root.path.join("index");
        fs::write(&index_path, &index).unwrap();

        assert!(
            seed_git_fsmonitor_index(
                &index_path,
                &root.path,
                20,
                &initial_baseline,
                &crate::scan::Invalidation::ExactPaths(Vec::new())
            )
            .unwrap_err()
            .contains("established continuity")
        );
        assert_eq!(
            fs::read(&index_path).unwrap(),
            index,
            "a rejected initial baseline must leave the Git index untouched"
        );
    }
}
