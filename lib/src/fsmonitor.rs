// Copyright 2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Filesystem monitor tool interface.
//!
//! Interfaces with a filesystem monitor tool to efficiently query for
//! filesystem updates, without having to crawl the entire working copy. This is
//! particularly useful for large working copies, or for working copies for
//! which it's expensive to materialize files, such those backed by a network or
//! virtualized filesystem.

#![warn(missing_docs)]

use std::path::PathBuf;
#[cfg(feature = "awacs")]
use std::sync::Arc;
#[cfg(feature = "awacs")]
use std::sync::Mutex;

use sha2::Digest as _;

use crate::config::ConfigGetError;
use crate::settings::UserSettings;

/// One external ignore input included in an AWACS cursor fingerprint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwacsExternalInput {
    /// Canonical raw-byte path used to read the input.
    pub path: Vec<u8>,
    /// Exact contents, or `None` when the selected input is absent.
    pub contents: Option<Vec<u8>>,
}

/// Canonical inputs whose changes can alter the tree produced by an AWACS
/// snapshot scan without changing files inside the leased snapshot root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwacsInputFingerprintV1 {
    /// Selected `core.excludesFile` or XDG ignore input.
    pub core_excludes: AwacsExternalInput,
    /// Colocated Git's `.git/info/exclude` input.
    pub git_info_exclude: AwacsExternalInput,
    /// Effective Git sparse mode and index-derived prefixes.
    pub git_sparse: Vec<u8>,
    /// Effective Jujutsu sparse prefixes.
    pub jj_sparse_prefixes: Vec<Vec<u8>>,
    /// Raw `snapshot.auto-track` expression after configuration resolution.
    pub snapshot_auto_track: Vec<u8>,
    /// Effective fileset aliases as `(name, expression)` byte pairs.
    pub fileset_aliases: Vec<(Vec<u8>, Vec<u8>)>,
    /// Effective new-file size limit.
    pub max_new_file_size: u64,
    /// Effective EOL conversion mode.
    pub eol_conversion: Vec<u8>,
    /// Effective executable-bit policy.
    pub exec_bit_policy: Vec<u8>,
}

impl AwacsInputFingerprintV1 {
    /// Version persisted next to an AWACS cursor.
    pub const VERSION: u32 = 1;

    /// Computes the domain-separated SHA-256 fingerprint for these inputs.
    pub fn sha256(&self) -> [u8; 32] {
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"jj:awacs-input-fingerprint:v1\0");
        put_external_input(&mut hasher, b"core-excludes", &self.core_excludes);
        put_external_input(&mut hasher, b"git-info-exclude", &self.git_info_exclude);
        put_bytes(&mut hasher, b"git-sparse", &self.git_sparse);

        let mut sparse_prefixes = self.jj_sparse_prefixes.clone();
        sparse_prefixes.sort_unstable();
        put_bytes_list(&mut hasher, b"jj-sparse-prefixes", &sparse_prefixes);
        put_bytes(
            &mut hasher,
            b"snapshot-auto-track",
            &self.snapshot_auto_track,
        );

        let mut aliases = self.fileset_aliases.clone();
        aliases.sort_unstable();
        put_u64(&mut hasher, b"fileset-alias-count", aliases.len() as u64);
        for (name, expression) in aliases {
            put_bytes(&mut hasher, b"fileset-alias-name", &name);
            put_bytes(&mut hasher, b"fileset-alias-expression", &expression);
        }
        put_u64(
            &mut hasher,
            b"snapshot-max-new-file-size",
            self.max_new_file_size,
        );
        put_bytes(&mut hasher, b"eol-conversion", &self.eol_conversion);
        put_bytes(&mut hasher, b"exec-bit-policy", &self.exec_bit_policy);
        hasher.finalize().into()
    }
}

fn put_external_input(hasher: &mut sha2::Sha256, label: &[u8], input: &AwacsExternalInput) {
    put_bytes(hasher, label, &input.path);
    match &input.contents {
        Some(contents) => {
            put_bytes(hasher, b"present", contents);
        }
        None => put_bytes(hasher, b"absent", &[]),
    }
}

fn put_bytes_list(hasher: &mut sha2::Sha256, label: &[u8], values: &[Vec<u8>]) {
    put_u64(hasher, label, values.len() as u64);
    for value in values {
        put_bytes(hasher, b"item", value);
    }
}

fn put_bytes(hasher: &mut sha2::Sha256, label: &[u8], value: &[u8]) {
    put_u64(hasher, b"label-len", label.len() as u64);
    hasher.update(label);
    put_u64(hasher, b"value-len", value.len() as u64);
    hasher.update(value);
}

fn put_u64(hasher: &mut sha2::Sha256, label: &[u8], value: u64) {
    hasher.update(label);
    hasher.update(value.to_be_bytes());
}

/// Config for Watchman filesystem monitor (<https://facebook.github.io/watchman/>).
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct WatchmanConfig {
    /// Whether to use triggers to monitor for changes in the background.
    pub register_trigger: bool,
}

/// Config for direct immutable snapshot scans through the `btrfs-awacs`
/// library.
pub struct AwacsConfig {
    /// Optional explicit AWACS socket. `None` lets the library discover the
    /// service for the live root and mount namespace.
    pub socket: Option<PathBuf>,
    /// Injectable crate-owned client used by direct-backend tests. Production
    /// configuration leaves this unset and uses library discovery.
    #[cfg(feature = "awacs")]
    pub client: Option<Arc<Mutex<Box<dyn btrfs_awacs::scan::ScanClient>>>>,
}

impl Clone for AwacsConfig {
    fn clone(&self) -> Self {
        Self {
            socket: self.socket.clone(),
            #[cfg(feature = "awacs")]
            client: self.client.clone(),
        }
    }
}

impl std::fmt::Debug for AwacsConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwacsConfig")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl PartialEq for AwacsConfig {
    fn eq(&self, other: &Self) -> bool {
        self.socket == other.socket
    }
}

impl Eq for AwacsConfig {}

/// The recognized kinds of filesystem monitors.
#[derive(Eq, PartialEq, Clone, Debug)]
pub enum FsmonitorSettings {
    /// The Watchman filesystem monitor (<https://facebook.github.io/watchman/>).
    Watchman(WatchmanConfig),

    /// Direct immutable snapshot scans through the `btrfs-awacs` library.
    Awacs(AwacsConfig),

    /// Only used in tests.
    Test {
        /// The set of changed files to pretend that the filesystem monitor is
        /// reporting.
        changed_files: Vec<PathBuf>,
        /// An alternate root to read while snapshotting. This models an
        /// immutable filesystem snapshot in tests.
        scan_root: Option<PathBuf>,
    },

    /// Only used in tests to model an immutable AWACS scan lease.
    TestAwacs {
        /// The synthetic immutable root to read while snapshotting.
        scan_root: PathBuf,
        /// The incremental paths to scan, or `None` for a full scan.
        changed_files: Option<Vec<PathBuf>>,
        /// The opaque AWACS cursor returned by the synthetic lease.
        cursor: Vec<u8>,
    },

    /// No filesystem monitor. This is the default if nothing is configured, but
    /// also makes it possible to turn off the monitor on a case-by-case basis
    /// when the user gives an option like `--config=fsmonitor.backend=none`;
    /// useful when e.g. doing analysis of snapshot performance.
    None,
}

impl FsmonitorSettings {
    /// Creates an `FsmonitorSettings` from a `config`.
    pub fn from_settings(settings: &UserSettings) -> Result<Self, ConfigGetError> {
        let name = "fsmonitor.backend";
        match settings.get_string(name)?.as_ref() {
            "watchman" => Ok(Self::Watchman(WatchmanConfig {
                register_trigger: settings
                    .get_bool("fsmonitor.watchman.register-snapshot-trigger")?,
            })),
            "awacs" => {
                #[cfg(all(target_os = "linux", feature = "awacs"))]
                {
                    let socket = settings.get_string("fsmonitor.awacs.socket")?;
                    Ok(Self::Awacs(AwacsConfig {
                        socket: (!socket.is_empty()).then(|| PathBuf::from(socket)),
                        client: None,
                    }))
                }
                #[cfg(not(all(target_os = "linux", feature = "awacs")))]
                {
                    Err(ConfigGetError::Type {
                        name: name.to_owned(),
                        error: "AWACS requires Linux and a jj build with the `awacs` feature"
                            .into(),
                        source_path: None,
                    })
                }
            }
            "test" => Err(ConfigGetError::Type {
                name: name.to_owned(),
                error: "Cannot use test fsmonitor in real repository".into(),
                source_path: None,
            }),
            "none" => Ok(Self::None),
            other => Err(ConfigGetError::Type {
                name: name.to_owned(),
                error: format!("Unknown fsmonitor kind: {other}").into(),
                source_path: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint_inputs() -> AwacsInputFingerprintV1 {
        AwacsInputFingerprintV1 {
            core_excludes: AwacsExternalInput {
                path: b"/home/user/.config/git/ignore".to_vec(),
                contents: Some(b"*.tmp\n".to_vec()),
            },
            git_info_exclude: AwacsExternalInput {
                path: b".git/info/exclude".to_vec(),
                contents: Some(b"generated/\n".to_vec()),
            },
            git_sparse: b"cone\0src/\0".to_vec(),
            jj_sparse_prefixes: vec![b"src".to_vec(), b"docs".to_vec()],
            snapshot_auto_track: b"all()".to_vec(),
            fileset_aliases: vec![
                (b"source".to_vec(), b"glob:src/**".to_vec()),
                (b"docs".to_vec(), b"glob:docs/**".to_vec()),
            ],
            max_new_file_size: 1024,
            eol_conversion: b"input-output".to_vec(),
            exec_bit_policy: b"track".to_vec(),
        }
    }

    #[test]
    fn awacs_fingerprint_is_canonical_for_unordered_inputs() {
        let inputs = fingerprint_inputs();
        let mut reordered = inputs.clone();
        reordered.jj_sparse_prefixes.reverse();
        reordered.fileset_aliases.reverse();
        assert_eq!(inputs.sha256(), reordered.sha256());
    }

    #[test]
    fn awacs_fingerprint_changes_for_external_input_changes() {
        let inputs = fingerprint_inputs();
        let mut changed = inputs.clone();
        changed.git_info_exclude.contents = Some(b"other/\n".to_vec());
        assert_ne!(inputs.sha256(), changed.sha256());

        let mut missing = inputs.clone();
        missing.core_excludes.contents = None;
        assert_ne!(inputs.sha256(), missing.sha256());
    }
}

/// Filesystem monitor integration using Watchman
/// (<https://facebook.github.io/watchman/>). Requires `watchman` to already be
/// installed on the system.
#[cfg(feature = "watchman")]
pub mod watchman {
    use std::path::Path;
    use std::path::PathBuf;

    use itertools::Itertools as _;
    use thiserror::Error;
    use tracing::info;
    use tracing::instrument;
    use watchman_client::expr;
    use watchman_client::prelude::Clock as InnerClock;
    use watchman_client::prelude::ClockSpec;
    use watchman_client::prelude::NameOnly;
    use watchman_client::prelude::QueryRequestCommon;
    use watchman_client::prelude::QueryResult;
    use watchman_client::prelude::SyncTimeout;
    use watchman_client::prelude::TriggerRequest;

    /// Represents an instance in time from the perspective of the filesystem
    /// monitor.
    ///
    /// This can be used to perform incremental queries. When making a query,
    /// the result will include an associated "clock" representing the time
    /// that the query was made. By passing the same clock into a future
    /// query, we inform the filesystem monitor that we only wish to get
    /// changed files since the previous point in time.
    #[derive(Clone, Debug)]
    pub struct Clock(InnerClock);

    impl From<crate::protos::local_working_copy::WatchmanClock> for Clock {
        fn from(clock: crate::protos::local_working_copy::WatchmanClock) -> Self {
            use crate::protos::local_working_copy::watchman_clock::WatchmanClock;
            let watchman_clock = clock.watchman_clock.unwrap();
            let clock = match watchman_clock {
                WatchmanClock::StringClock(string_clock) => {
                    InnerClock::Spec(ClockSpec::StringClock(string_clock))
                }
                WatchmanClock::UnixTimestamp(unix_timestamp) => {
                    InnerClock::Spec(ClockSpec::UnixTimestamp(unix_timestamp))
                }
            };
            Self(clock)
        }
    }

    impl From<Clock> for crate::protos::local_working_copy::WatchmanClock {
        fn from(clock: Clock) -> Self {
            use crate::protos::local_working_copy::watchman_clock;
            let Clock(clock) = clock;
            let watchman_clock = match clock {
                InnerClock::Spec(ClockSpec::StringClock(string_clock)) => {
                    watchman_clock::WatchmanClock::StringClock(string_clock)
                }
                InnerClock::Spec(ClockSpec::UnixTimestamp(unix_timestamp)) => {
                    watchman_clock::WatchmanClock::UnixTimestamp(unix_timestamp)
                }
                InnerClock::ScmAware(_) => {
                    unimplemented!("SCM-aware Watchman clocks not supported")
                }
            };
            Self {
                watchman_clock: Some(watchman_clock),
            }
        }
    }

    #[expect(missing_docs)]
    #[derive(Debug, Error)]
    pub enum Error {
        #[error("Could not connect to Watchman")]
        WatchmanConnectError(#[source] watchman_client::Error),

        #[error("Could not canonicalize working copy root path")]
        CanonicalizeRootError(#[source] std::io::Error),

        #[error("Watchman failed to resolve the working copy root path")]
        ResolveRootError(#[source] watchman_client::Error),

        #[error("Failed to query Watchman")]
        WatchmanQueryError(#[source] watchman_client::Error),

        #[error("Failed to register Watchman trigger")]
        WatchmanTriggerError(#[source] watchman_client::Error),
    }

    impl Error {
        /// Formats the most actionable user-facing detail from this Watchman
        /// error.
        pub fn detailed_message(&self) -> String {
            match self {
                Self::WatchmanConnectError(err)
                | Self::ResolveRootError(err)
                | Self::WatchmanQueryError(err)
                | Self::WatchmanTriggerError(err) => err.to_string(),
                Self::CanonicalizeRootError(err) => {
                    format!("Could not canonicalize working copy root path: {err}")
                }
            }
        }
    }

    /// Handle to the underlying Watchman instance.
    pub struct Fsmonitor {
        client: watchman_client::Client,
        resolved_root: watchman_client::ResolvedRoot,
    }

    impl Fsmonitor {
        /// Initialize the Watchman filesystem monitor. If it's not already
        /// running, this will start it and have it crawl the working
        /// copy to build up its in-memory representation of the
        /// filesystem, which may take some time.
        #[instrument]
        pub async fn init(
            working_copy_path: &Path,
            config: &super::WatchmanConfig,
        ) -> Result<Self, Error> {
            info!("Initializing Watchman filesystem monitor...");
            let connector = watchman_client::Connector::new();
            let client = connector
                .connect()
                .await
                .map_err(Error::WatchmanConnectError)?;
            let working_copy_root = watchman_client::CanonicalPath::canonicalize(working_copy_path)
                .map_err(Error::CanonicalizeRootError)?;
            let resolved_root = client
                .resolve_root(working_copy_root)
                .await
                .map_err(Error::ResolveRootError)?;

            let monitor = Self {
                client,
                resolved_root,
            };

            // Registering the trigger causes an unconditional evaluation of the query, so
            // test if it is already registered first.
            if !config.register_trigger {
                monitor.unregister_trigger().await?;
            } else if !monitor.is_trigger_registered().await? {
                monitor.register_trigger().await?;
            }
            Ok(monitor)
        }

        /// Query for changed files since the previous point in time.
        ///
        /// The returned list of paths is relative to the `working_copy_path`.
        /// If it is `None`, then the caller must crawl the entire working copy
        /// themselves.
        #[instrument(skip(self))]
        pub async fn query_changed_files(
            &self,
            previous_clock: Option<Clock>,
        ) -> Result<(Clock, Option<Vec<PathBuf>>), Error> {
            // TODO: might be better to specify query options by caller, but we
            // shouldn't expose the underlying watchman API too much.
            info!("Querying Watchman for changed files...");
            let QueryResult {
                version: _,
                is_fresh_instance,
                files,
                clock,
                state_enter: _,
                state_leave: _,
                state_metadata: _,
                saved_state_info: _,
                debug: _,
            }: QueryResult<NameOnly> = self
                .client
                .query(
                    &self.resolved_root,
                    QueryRequestCommon {
                        since: previous_clock.map(|Clock(clock)| clock),
                        expression: Some(self.build_exclude_expr()),
                        ..Default::default()
                    },
                )
                .await
                .map_err(Error::WatchmanQueryError)?;

            let clock = Clock(clock);
            if is_fresh_instance {
                // The Watchman documentation states that if it was a fresh
                // instance, we need to delete any tree entries that didn't appear
                // in the returned list of changed files. For now, the caller will
                // handle this by manually crawling the working copy again.
                Ok((clock, None))
            } else {
                let paths = files
                    .unwrap_or_default()
                    .into_iter()
                    .map(|NameOnly { name }| name.into_inner())
                    .collect_vec();
                Ok((clock, Some(paths)))
            }
        }

        /// Return a synchronized clock without enumerating files.
        ///
        /// Callers can persist this after independently establishing that
        /// their working-copy tree matches the filesystem at this point.
        #[instrument(skip(self))]
        pub async fn clock(&self) -> Result<Clock, Error> {
            let clock = self
                .client
                .clock(&self.resolved_root, SyncTimeout::Default)
                .await
                .map_err(Error::WatchmanQueryError)?;
            Ok(Clock(InnerClock::Spec(clock)))
        }

        /// Return whether or not a trigger has been registered already.
        #[instrument(skip(self))]
        pub async fn is_trigger_registered(&self) -> Result<bool, Error> {
            info!("Checking for an existing Watchman trigger...");
            Ok(self
                .client
                .list_triggers(&self.resolved_root)
                .await
                .map_err(Error::WatchmanTriggerError)?
                .triggers
                .iter()
                .any(|t| t.name == "jj-background-monitor"))
        }

        /// Register trigger for changed files.
        #[instrument(skip(self))]
        async fn register_trigger(&self) -> Result<(), Error> {
            info!("Registering Watchman trigger...");
            let null = if cfg!(windows) { ">NUL" } else { ">/dev/null" };
            self.client
                .register_trigger(
                    &self.resolved_root,
                    TriggerRequest {
                        name: "jj-background-monitor".to_string(),
                        command: vec![
                            "jj".to_string(),
                            "--quiet".to_string(),
                            "util".to_string(),
                            "snapshot".to_string(),
                        ],
                        expression: Some(self.build_exclude_expr()),
                        stderr: Some(null.into()),
                        stdout: Some(null.into()),
                        ..Default::default()
                    },
                )
                .await
                .map_err(Error::WatchmanTriggerError)?;
            Ok(())
        }

        /// Register trigger for changed files.
        #[instrument(skip(self))]
        async fn unregister_trigger(&self) -> Result<(), Error> {
            info!("Unregistering Watchman trigger...");
            self.client
                .remove_trigger(&self.resolved_root, "jj-background-monitor")
                .await
                .map_err(Error::WatchmanTriggerError)?;
            Ok(())
        }

        /// Build an exclude expr for `working_copy_path`.
        fn build_exclude_expr(&self) -> expr::Expr {
            // TODO: consider parsing `.gitignore`.
            let exclude_dirs = [Path::new(".git"), Path::new(".jj")];
            let excludes = itertools::chain(
                // the directories themselves
                [expr::Expr::Name(expr::NameTerm {
                    paths: exclude_dirs.iter().map(|&name| name.to_owned()).collect(),
                    wholename: true,
                })],
                // and all files under the directories
                exclude_dirs.iter().map(|&name| {
                    expr::Expr::DirName(expr::DirNameTerm {
                        path: name.to_owned(),
                        depth: None,
                    })
                }),
            )
            .collect();
            expr::Expr::Not(Box::new(expr::Expr::Any(excludes)))
        }
    }
}
