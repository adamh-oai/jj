// Copyright 2026 The Jujutsu Authors
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

//! Endpoint preparation, frozen transfers, and independent local publication.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::io;
use std::io::Read;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use jj_lib::config::ConfigGetResultExt as _;
use jj_lib::lock::FileLock;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::op_store::ViewId;
use jj_lib::repo::Repo as _;
use jj_lib::settings::HumanByteSize;
use jj_lib::simple_op_store::SimpleOpStore;
use jj_lib::sync::SyncError;
use jj_lib::sync::endpoint::Endpoint;
use jj_lib::sync::endpoint::EndpointError;
use jj_lib::sync::endpoint::NativeCapture;
use jj_lib::sync::metadata::KnownCommitMetadata;
use jj_lib::sync::metadata::MetadataInventory;
use jj_lib::sync::metadata::MetadataSummary;
use jj_lib::sync::native;
use jj_lib::sync::native::prepare_integration;
#[cfg(unix)]
use jj_lib::sync::objects::ImmutableFiles;
use jj_lib::sync::objects::Inventory;
use jj_lib::sync::objects::Quarantine;
use jj_lib::sync::objects::QuarantineLimits;
use jj_lib::sync::objects::Record;
use jj_lib::sync::objects::RecordId;
use jj_lib::sync::objects::RepositoryObjects;
#[cfg(unix)]
use jj_lib::sync::packed::GitPackRepository;
use jj_lib::sync::protocol::Direction;
use jj_lib::sync::protocol::Hello;
use jj_lib::sync::protocol::Message;
use jj_lib::sync::protocol::PROTOCOL_VERSION;
use jj_lib::sync::protocol::Session;
use jj_lib::sync::shared::SharedEventId;
use jj_lib::sync::state::SessionPhase;
use jj_lib::sync::state::SyncSession;
use jj_lib::workspace::Workspace;
use serde::Deserialize;
use serde::Serialize;

use crate::cli_util::CommandHelper;
use crate::cli_util::WorkspaceCommandHelper;
use crate::cli_util::print_snapshot_stats;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::command_error::user_error_with_message;
use crate::progress::ProgressWriter;
#[cfg(unix)]
use crate::sync_process::ProcessTransferOptions;
#[cfg(unix)]
use crate::sync_process::ProgressCallback;
#[cfg(unix)]
use crate::sync_rsync_transport::TransferMode;
use crate::sync_transport;
use crate::sync_transport::Transport;
use crate::ui::Ui;

const CAPABILITIES: [&str; 4] = [
    "raw-history-v1",
    "native-operation-graph-v1",
    "streaming-objects-v1",
    "local-workspaces-v1",
];
const INITIALIZE_RECEIVER_CAPABILITY: &str = "initialize-receiver-v1";
const NATIVE_GIT_CAPABILITY: &str = "native-git-pack-v1";
const ADAPTIVE_METADATA_CAPABILITY: &str = "adaptive-commit-metadata-v1";
const RSYNC_IMMUTABLE_CAPABILITY: &str = "rsync-immutable-v1";
const METADATA_PROBE_BATCH_SIZE: usize = 128;
const NEGOTIATE_OBJECT_HASH: &str = "auto";
const INVENTORY_BATCH_SIZE: usize = 512;
const MAX_INVENTORY_BATCH_BYTES: usize = INVENTORY_BATCH_SIZE * 192;
// Phase controls contain captured roots or publication acknowledgments. Object
// inventories and bodies use separate batches and streams, regardless of size.
const MAX_PHASE_CONTROL_BYTES: usize = 64 * 1024;
// Known IDs only avoid retransmission. Bound this peer-controlled cache and
// resend anything omitted from it, so large repositories remain supported.
const MAX_KNOWN_RECORD_IDS: usize = 65_536;

#[derive(Clone, Copy)]
struct SyncProgress {
    label: &'static str,
}

impl SyncProgress {
    fn phase(self, ui: &Ui, message: impl std::fmt::Display) -> io::Result<()> {
        // Unlike the delayed terminal counter, phase changes are also useful
        // before a blocking step and when stderr is redirected.
        writeln!(ui.status(), "{} {message}...", self.label)
    }

    #[cfg(unix)]
    fn transfer(self, ui: &Ui, action: &'static str) -> Option<Box<ProgressCallback<'static>>> {
        let writer = Mutex::new(ProgressWriter::new(ui, self.label)?);
        Some(Box::new(move |sent_bytes, received_bytes| {
            if let Ok(mut writer) = writer.try_lock() {
                writer
                    .display(&format!(
                        "{action}: {:.1} MiB sent, {:.1} MiB received",
                        sent_bytes as f64 / (1024.0 * 1024.0),
                        received_bytes as f64 / (1024.0 * 1024.0),
                    ))
                    .ok();
            }
        }))
    }
}

struct TransferProgress<'a> {
    root: &'a Path,
    action: &'static str,
    writer: Option<ProgressWriter<'static>>,
    objects: usize,
    total_objects: Option<usize>,
    transferred_bytes: u64,
    commit_metadata_records: usize,
    started: Instant,
    next_update: Instant,
}

impl<'a> TransferProgress<'a> {
    fn new(ui: &Ui, root: &'a Path, action: &'static str, total_objects: Option<usize>) -> Self {
        let started = Instant::now();
        Self {
            root,
            action,
            writer: ProgressWriter::new(ui, "Synchronizing"),
            objects: 0,
            total_objects,
            transferred_bytes: 0,
            commit_metadata_records: 0,
            started,
            next_update: started + Duration::from_secs(1),
        }
    }

    fn record(&mut self, record: &Record, encoded_bytes: usize) {
        if !matches!(record, Record::GitHeader(_) | Record::GitChunk { .. }) {
            self.objects += 1;
        }
        self.transferred_bytes = self.transferred_bytes.saturating_add(encoded_bytes as u64);
        self.commit_metadata_records += usize::from(matches!(record, Record::GitCommitMetadata(_)));
        let now = Instant::now();
        if now >= self.next_update {
            self.report(false);
            self.next_update = now + Duration::from_secs(1);
        }
    }

    fn finish(mut self) {
        self.report(true);
    }

    fn report(&mut self, complete: bool) {
        tracing::debug!(
            path = %self.root.display(),
            action = self.action,
            completed_objects = self.objects,
            total_objects = ?self.total_objects,
            transferred_bytes = self.transferred_bytes,
            commit_metadata_records = self.commit_metadata_records,
            elapsed_seconds = self.started.elapsed().as_secs_f64(),
            complete,
            "Synchronization transfer progress"
        );
        if let Some(writer) = &mut self.writer {
            let objects = match self.total_objects {
                Some(total) => format!("{}/{total}", self.objects),
                None => self.objects.to_string(),
            };
            writer
                .display(&format!(
                    "{}: {objects} objects, {:.1} MiB transferred",
                    self.action,
                    self.transferred_bytes as f64 / (1024.0 * 1024.0)
                ))
                .ok();
        }
    }
}

trait Stream {
    fn send(&mut self, message: &Message) -> Result<(), CommandError>;
    fn receive(&mut self) -> Result<Message, CommandError>;

    #[cfg(unix)]
    fn transfer_git(
        &mut self,
        _git: &Path,
        _repository: &Path,
        _sending: bool,
        _options: ProcessTransferOptions<'_>,
    ) -> Result<crate::sync_process::ProcessTransferReport, CommandError> {
        Err(user_error("This stream cannot transfer native Git data"))
    }

    #[cfg(unix)]
    fn transfer_immutable(
        &mut self,
        _rsync: &Path,
        _directory: &Path,
        _basis: Option<&Path>,
        _mode: TransferMode<'_>,
        _options: ProcessTransferOptions<'_>,
    ) -> Result<crate::sync_process::ProcessTransferReport, CommandError> {
        Err(user_error(
            "This stream cannot transfer immutable history files",
        ))
    }
}

impl Stream for Transport {
    fn send(&mut self, message: &Message) -> Result<(), CommandError> {
        Self::send(self, message).map_err(failed)
    }

    fn receive(&mut self) -> Result<Message, CommandError> {
        Self::receive(self)
            .map_err(failed)
            .and_then(check_peer_error)
    }

    #[cfg(unix)]
    fn transfer_git(
        &mut self,
        git: &Path,
        repository: &Path,
        sending: bool,
        options: ProcessTransferOptions<'_>,
    ) -> Result<crate::sync_process::ProcessTransferReport, CommandError> {
        use crate::sync_git_transport::GitAction;
        let action = if sending {
            GitAction::Push
        } else {
            GitAction::Fetch
        };
        let (reader, writer) = self.framed_io_mut().map_err(failed)?;
        crate::sync_git_transport::run_client(git, repository, action, options, reader, writer)
    }

    #[cfg(unix)]
    fn transfer_immutable(
        &mut self,
        rsync: &Path,
        directory: &Path,
        basis: Option<&Path>,
        mode: TransferMode<'_>,
        options: ProcessTransferOptions<'_>,
    ) -> Result<crate::sync_process::ProcessTransferReport, CommandError> {
        let (reader, writer) = self.framed_io_mut().map_err(failed)?;
        crate::sync_rsync_transport::run(rsync, directory, basis, mode, options, reader, writer)
    }
}

impl<R: Read + Send, W: io::Write> Stream for Session<R, W> {
    fn send(&mut self, message: &Message) -> Result<(), CommandError> {
        Self::send(self, message).map_err(failed)
    }

    fn receive(&mut self) -> Result<Message, CommandError> {
        Self::receive(self)
            .map_err(failed)
            .and_then(check_peer_error)
    }

    #[cfg(unix)]
    fn transfer_git(
        &mut self,
        git: &Path,
        repository: &Path,
        sending: bool,
        options: ProcessTransferOptions<'_>,
    ) -> Result<crate::sync_process::ProcessTransferReport, CommandError> {
        use crate::sync_git_transport::GitService;
        let service = if sending {
            GitService::UploadPack
        } else {
            GitService::ReceivePack
        };
        let (reader, writer) = self.split_mut();
        crate::sync_git_transport::run_server(git, repository, service, options, reader, writer)
    }

    #[cfg(unix)]
    fn transfer_immutable(
        &mut self,
        rsync: &Path,
        directory: &Path,
        basis: Option<&Path>,
        mode: TransferMode<'_>,
        options: ProcessTransferOptions<'_>,
    ) -> Result<crate::sync_process::ProcessTransferReport, CommandError> {
        let (reader, writer) = self.split_mut();
        crate::sync_rsync_transport::run(rsync, directory, basis, mode, options, reader, writer)
    }
}

fn check_peer_error(message: Message) -> Result<Message, CommandError> {
    if let Message::Error(error) = message {
        Err(user_error(format!("Peer synchronization failed: {error}")))
    } else {
        Ok(message)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Advertisement {
    peer: String,
    raw_heads: Vec<String>,
    current_operation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum WireRecordId {
    Operation(String),
    View(String),
    SharedEvent(String),
    Git(String),
}

impl From<&RecordId> for WireRecordId {
    fn from(value: &RecordId) -> Self {
        match value {
            RecordId::Operation(id) => Self::Operation(id.hex()),
            RecordId::View(id) => Self::View(id.hex()),
            RecordId::SharedEvent(id) => Self::SharedEvent(id.hex()),
            RecordId::Git(id) => Self::Git(id.to_string()),
        }
    }
}

impl WireRecordId {
    fn decode(&self) -> Result<RecordId, CommandError> {
        match self {
            Self::Operation(id) => Ok(RecordId::Operation(parse_raw(id)?)),
            Self::View(id) => {
                check_id(id)?;
                Ok(RecordId::View(ViewId::try_from_hex(id).unwrap()))
            }
            Self::SharedEvent(id) => Ok(RecordId::SharedEvent(parse_shared(id)?)),
            Self::Git(id) => gix::ObjectId::from_hex(id.as_bytes())
                .map(RecordId::Git)
                .map_err(|err| user_error(format!("Invalid Git identity from peer: {err}"))),
        }
    }
}

impl Advertisement {
    fn raw_heads(&self) -> Result<Vec<OperationId>, CommandError> {
        if self.raw_heads.is_empty() {
            return Err(user_error("Peer advertised no original operation heads"));
        }
        self.raw_heads.iter().map(|id| parse_raw(id)).collect()
    }

    fn validate(&self) -> Result<(), CommandError> {
        if self.peer.is_empty() || self.peer.len() > 1024 {
            return Err(user_error("Peer advertised an invalid endpoint identity"));
        }
        self.raw_heads()?;
        if !self
            .raw_heads()?
            .contains(&parse_raw(&self.current_operation)?)
        {
            return Err(user_error(
                "Peer's current operation is not a captured head",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Applied {
    operation: String,
    captured_operation: String,
    current_operation: String,
}

struct PreparedEndpoint {
    helper: WorkspaceCommandHelper,
    endpoint: Endpoint,
    capture: NativeCapture,
    inventory: Inventory,
    root: PathBuf,
    progress: SyncProgress,
    ignore_working_copy: bool,
    transfer: String,
    _transfer_lease: FileLock,
    pins: Vec<String>,
    publication_leases: Vec<FileLock>,
    packed_git: bool,
    rsync_immutable: bool,
    #[cfg(unix)]
    immutable_basis: Option<ImmutableFiles>,
    common_metadata: KnownCommitMetadata,
}

impl PreparedEndpoint {
    async fn open(
        ui: &Ui,
        command: &CommandHelper,
        root: &Path,
        ignore_working_copy: bool,
        packed_git: bool,
        rsync_immutable: bool,
        progress: SyncProgress,
    ) -> Result<Self, CommandError> {
        tracing::debug!(
            path = %root.display(),
            ignore_working_copy,
            "Preparing synchronization endpoint"
        );
        progress.phase(ui, "Preparing repository")?;
        let transfer = format!("{:032x}", rand::random::<u128>());
        let mut helper = load_helper(ui, command, root, ignore_working_copy).await?;
        jj_lib::git::get_git_backend(helper.repo().store())?;
        if !matches!(
            helper.repo().op_store().name(),
            SimpleOpStore::NAME | SimpleOpStore::SYNC_NAME | SimpleOpStore::NATIVE_SYNC_NAME
        ) {
            return Err(user_error(
                "Repository synchronization requires the built-in operation store",
            ));
        }
        let root = helper.workspace_root().to_owned();
        if helper.repo().op_store().name() == SimpleOpStore::NAME {
            progress.phase(
                ui,
                "Enabling synchronization; waiting for repository access",
            )?;
            // Release this command's shared format access before requesting
            // exclusive access. Another command may finish the transition while
            // we wait, so enable() checks the marker again under its lock.
            let repo_path = helper.repo_path().to_owned();
            drop(helper);
            #[cfg(feature = "test-fakes")]
            crate::cli_util::pause_for_test("JJ_TEST_SYNC_BEFORE_FORMAT_TRANSITION_PAUSE_DIR");
            Endpoint::enable_native(&repo_path).map_err(failed)?;
            helper = load_helper(ui, command, &root, ignore_working_copy).await?;
        }
        let endpoint = Endpoint::load(helper.repo_path());
        if helper.repo().op_store().name() == SimpleOpStore::SYNC_NAME {
            progress.phase(ui, "Migrating retained history to ordinary operations")?;
            migrate_legacy(ui, &mut helper, &endpoint).await?;
            helper = load_helper(ui, command, &root, ignore_working_copy).await?;
        }
        let transfer_lease = endpoint.acquire_transfer_lease(&transfer).map_err(failed)?;
        endpoint
            .retire_inactive_native_transfers()
            .map_err(failed)?;
        {
            // Establish the receiver's physical observations before any peer
            // view can change the corresponding shared remote-tag targets.
            let mut tx = helper.repo().start_transaction();
            jj_lib::git::record_remote_tag_observations(tx.repo_mut())?;
            if tx.repo().has_changes() {
                helper
                    .finish_sync_transaction(
                        ui,
                        tx,
                        "initialize synchronization observations",
                        |_, _| Ok(()),
                    )
                    .await?;
            }
        }
        progress.phase(ui, "Reading operation history")?;
        endpoint
            .recover_published(helper.repo())
            .await
            .map_err(failed)?;
        retire_interrupted_publications(&endpoint, helper.repo()).await?;
        let capture = capture_current(
            ui,
            command,
            &mut helper,
            &endpoint,
            &root,
            ignore_working_copy,
            &transfer,
        )
        .await?;
        progress.phase(ui, "Collecting history objects")?;
        let inventory = collect_inventory(&helper, &endpoint, &capture.raw_heads, &[], packed_git)?;
        let pins = vec![
            transfer.clone(),
            format!("{transfer}-incoming"),
            format!("{transfer}-selected"),
        ];
        #[cfg(unix)]
        let (pins, immutable_basis) = if rsync_immutable {
            progress.phase(ui, "Preparing history comparison")?;
            let mut pins = pins;
            let key = format!("{transfer}-basis");
            let (raw_heads, shared_heads) = endpoint.pin_retained(&key).map_err(failed)?;
            pins.push(key);
            let retained = collect_inventory(&helper, &endpoint, &raw_heads, &shared_heads, true)?;
            let basis = objects(&helper, &endpoint)?
                .prepare_immutable_files(&helper.repo_path().join("sync"), &retained)
                .map_err(failed)?;
            (pins, Some(basis))
        } else {
            (pins, None)
        };
        Ok(Self {
            helper,
            endpoint,
            capture,
            inventory,
            root,
            progress,
            ignore_working_copy,
            pins,
            publication_leases: Vec::new(),
            transfer,
            _transfer_lease: transfer_lease,
            packed_git,
            rsync_immutable,
            #[cfg(unix)]
            immutable_basis,
            common_metadata: KnownCommitMetadata::new(),
        })
    }

    fn hello(&self, direction: Direction, path: Vec<u8>) -> Result<Hello, CommandError> {
        profile_hello(
            self.helper.workspace(),
            direction,
            path,
            self.ignore_working_copy,
        )
    }

    fn advertisement(&self) -> Result<Advertisement, CommandError> {
        self.advertise_inventory(&self.inventory, &self.capture.raw_heads[0])
    }

    fn advertise_inventory(
        &self,
        inventory: &Inventory,
        selected: &OperationId,
    ) -> Result<Advertisement, CommandError> {
        Ok(Advertisement {
            peer: self.endpoint.identity().map_err(failed)?,
            raw_heads: inventory.raw_heads.iter().map(|id| id.hex()).collect(),
            current_operation: selected.hex(),
        })
    }

    fn send_offer(
        &self,
        ui: &Ui,
        stream: &mut impl Stream,
        inventory: &Inventory,
        selected: &OperationId,
        known: &BTreeSet<RecordId>,
    ) -> Result<(), CommandError> {
        tracing::debug!(
            path = %self.root.display(),
            "Sending synchronization objects"
        );
        send_command(
            stream,
            "offer",
            &self.advertise_inventory(inventory, selected)?,
        )?;
        self.progress.phase(ui, "Preparing objects to send")?;
        let objects = objects(&self.helper, &self.endpoint)?;
        #[cfg(unix)]
        let git_repository = (self.packed_git
            && (!inventory.git_commits.is_empty()
                || !inventory.legacy_tree_roots.is_empty()
                || !inventory.git_roots.is_empty()))
        .then(|| {
            GitPackRepository::sender(
                &self.helper.repo_path().join("sync"),
                jj_lib::git::get_git_backend(self.helper.repo().store())?,
                inventory,
                &self.quarantine_limits()?,
            )
            .map_err(failed)
        })
        .transpose()?;
        #[cfg(unix)]
        if self.rsync_immutable {
            let files = objects
                .prepare_immutable_files(&self.helper.repo_path().join("sync"), inventory)
                .map_err(failed)?;
            for mode in [TransferMode::SendPlan, TransferMode::Send] {
                self.transfer_immutable(ui, stream, files.path(), None, mode, u64::MAX)?;
            }
        }
        self.progress.phase(
            ui,
            if self.rsync_immutable {
                "Sending commit metadata"
            } else {
                "Sending history objects"
            },
        )?;
        let mut progress = TransferProgress::new(
            ui,
            &self.root,
            "Sending",
            self.common_metadata.is_empty().then(|| {
                inventory
                    .missing(known)
                    .filter(|id| {
                        (!self.rsync_immutable || matches!(id, RecordId::Git(_)))
                            && (!self.packed_git
                                || match id {
                                    RecordId::Git(id) => inventory.git_commits.contains(
                                        &jj_lib::backend::CommitId::from_bytes(id.as_bytes()),
                                    ),
                                    _ => true,
                                })
                    })
                    .count()
            }),
        );
        let mut emit = |record: Record| {
            if self.rsync_immutable && !matches!(record, Record::GitCommitMetadata(_)) {
                return Ok(());
            }
            let bytes = record.encode()?;
            let encoded_bytes = bytes.len();
            stream
                .send(&Message::Record(bytes))
                .map_err(|err| SyncError::InvalidData(command_error_message(&err)))?;
            progress.record(&record, encoded_bytes);
            Ok(())
        };
        if self.rsync_immutable {
            objects
                .export_commit_metadata_records(inventory, &self.common_metadata, &mut emit)
                .map_err(failed)?;
        } else if self.packed_git {
            objects
                .export_metadata_records(inventory, known, &self.common_metadata, &mut emit)
                .map_err(failed)?;
        } else {
            for id in inventory.missing(known) {
                objects.export_records(id, &mut emit).map_err(failed)?;
            }
        }
        stream.send(&Message::Done(Vec::new()))?;
        progress.finish();
        #[cfg(unix)]
        if self.packed_git {
            send_command(stream, "git-pack", &git_repository.is_some())?;
            if let Some(repository) = git_repository {
                self.transfer_git(ui, stream, repository.path(), true, u64::MAX)?;
            }
        }
        Ok(())
    }

    fn receive_offer(
        &self,
        ui: &Ui,
        stream: &mut impl Stream,
        expected_peer: &str,
        captured: Option<&Advertisement>,
    ) -> Result<Advertisement, CommandError> {
        tracing::debug!(
            path = %self.root.display(),
            "Receiving synchronization objects"
        );
        let offered: Advertisement = receive_command(stream, "offer")?;
        offered.validate()?;
        if offered.peer != expected_peer {
            return Err(user_error("Peer identity changed during synchronization"));
        }
        if let Some(captured) = captured {
            check_captured_offer(&offered, captured)?;
        }
        self.progress.phase(ui, "Preparing to receive objects")?;
        let objects = objects(&self.helper, &self.endpoint)?;
        let mut quarantine = Quarantine::with_limits(
            &self.helper.repo_path().join("sync"),
            self.helper.repo().store().root_commit_id().clone(),
            self.quarantine_limits()?,
        )
        .map_err(failed)?;
        #[cfg(unix)]
        let git_repository_path = if self.packed_git {
            let repository = GitPackRepository::receiver(
                &self.helper.repo_path().join("sync"),
                jj_lib::git::get_git_backend(self.helper.repo().store())?,
                &self.inventory,
                &self.quarantine_limits()?,
            )
            .map_err(failed)?;
            let path = repository.path().to_owned();
            quarantine
                .set_git_pack_repository(repository)
                .map_err(failed)?;
            Some(path)
        } else {
            None
        };
        if !self.common_metadata.is_empty() {
            quarantine
                .set_known_commit_metadata(self.common_metadata.clone())
                .map_err(failed)?;
        }
        #[cfg(unix)]
        if self.rsync_immutable {
            let parent = self.helper.repo_path().join("sync");
            let basis = self
                .immutable_basis
                .as_ref()
                .expect("rsync comparison snapshot was prepared");
            let incoming = tempfile::Builder::new()
                .prefix("jj-sync-immutable-incoming-")
                .tempdir_in(&parent)?;
            let listing = tempfile::NamedTempFile::new_in(&parent)?;
            let limits = self.quarantine_limits()?;
            // File lists describe existing history too. Bound their overhead
            // separately; actual incoming payloads are checked before install.
            let allowance = 16 * 1024 * 1024;
            self.transfer_immutable(
                ui,
                stream,
                incoming.path(),
                Some(basis.path()),
                TransferMode::Plan {
                    output: listing.path(),
                    max_bytes: (limits.max_records as u64)
                        .saturating_mul(256)
                        .saturating_add(16 * 1024)
                        .min(quarantine.remaining_packed_bytes()),
                },
                quarantine
                    .remaining_packed_bytes()
                    .saturating_add(allowance),
            )?;
            let plan = quarantine
                .plan_immutable_files(listing.path())
                .map_err(failed)?;
            let mut files_from = tempfile::NamedTempFile::new_in(&parent)?;
            plan.write_paths(files_from.as_file_mut()).map_err(failed)?;
            tracing::debug!(
                path = %self.root.display(),
                planned_files = plan.len(),
                planned_bytes = plan.data_bytes(),
                "Immutable history file transfer planned"
            );
            self.progress.phase(
                ui,
                format_args!(
                    "Receiving {} history files ({:.1} MiB)",
                    plan.len(),
                    plan.data_bytes() as f64 / (1024.0 * 1024.0)
                ),
            )?;
            self.transfer_immutable(
                ui,
                stream,
                incoming.path(),
                Some(basis.path()),
                TransferMode::Receive {
                    files_from: files_from.path(),
                    max_file_size: plan.max_file_size(),
                },
                // The reserved budget includes per-file allocation overhead,
                // which also covers large lists of tiny requested objects.
                quarantine
                    .remaining_packed_bytes()
                    .saturating_add(allowance),
            )?;
            let copied_files = quarantine
                .stage_immutable_files(&objects, incoming.path(), &plan)
                .map_err(failed)?;
            tracing::debug!(
                path = %self.root.display(),
                copied_files,
                "Immutable history files staged"
            );
        }
        self.progress.phase(
            ui,
            if self.rsync_immutable {
                "Receiving commit metadata"
            } else {
                "Receiving history objects"
            },
        )?;
        let mut progress = TransferProgress::new(ui, &self.root, "Receiving", None);
        loop {
            match stream.receive()? {
                Message::Record(bytes) => {
                    let record = Record::decode(&bytes).map_err(failed)?;
                    if self.rsync_immutable && !matches!(record, Record::GitCommitMetadata(_)) {
                        return Err(user_error(
                            "Unexpected object record after immutable history file transfer",
                        ));
                    }
                    quarantine.stage(&objects, &record).map_err(failed)?;
                    progress.record(&record, bytes.len());
                }
                Message::Done(bytes) if bytes.is_empty() => break,
                _ => {
                    return Err(user_error(
                        "Unexpected message during synchronization object transfer",
                    ));
                }
            }
        }
        progress.finish();
        #[cfg(unix)]
        if let Some(path) = git_repository_path {
            let has_git_objects: bool = receive_command(stream, "git-pack")?;
            if has_git_objects {
                self.transfer_git(
                    ui,
                    stream,
                    &path,
                    false,
                    quarantine.remaining_packed_bytes(),
                )?;
            }
        }
        tracing::debug!(
            path = %self.root.display(),
            "Validating received synchronization history"
        );
        self.progress.phase(ui, "Validating received history")?;
        let inventory = quarantine
            .validate(&objects, &offered.raw_heads()?, &[])
            .map_err(failed)?;
        tracing::debug!(
            path = %self.root.display(),
            objects = inventory.records.len(),
            "Received synchronization history validated"
        );
        tracing::debug!(
            path = %self.root.display(),
            "Installing synchronized objects"
        );
        self.progress.phase(ui, "Installing received objects")?;
        self.endpoint
            .install_native(
                self.helper.repo(),
                &quarantine,
                &inventory,
                &format!("{}-incoming", self.transfer),
            )
            .map_err(failed)?;
        tracing::debug!(
            path = %self.root.display(),
            objects = inventory.records.len(),
            "Synchronized objects installed"
        );
        Ok(offered)
    }

    #[cfg(unix)]
    fn transfer_immutable(
        &self,
        ui: &Ui,
        stream: &mut impl Stream,
        directory: &Path,
        basis: Option<&Path>,
        mode: TransferMode<'_>,
        max_received_bytes: u64,
    ) -> Result<(), CommandError> {
        let sending = matches!(mode, TransferMode::SendPlan | TransferMode::Send);
        let planning = matches!(mode, TransferMode::Plan { .. } | TransferMode::SendPlan);
        if planning {
            self.progress.phase(ui, "Comparing operation history")?;
        } else if sending {
            self.progress.phase(ui, "Sending operations and views")?;
        }
        tracing::debug!(
            path = %self.root.display(),
            sending,
            planning,
            "Transferring immutable history files with rsync"
        );
        let progress = self.progress.transfer(
            ui,
            if planning {
                "Comparing history"
            } else {
                "Transferring history"
            },
        );
        let report = stream.transfer_immutable(
            Path::new("rsync"),
            directory,
            basis,
            mode,
            ProcessTransferOptions {
                max_received_bytes,
                progress: progress.as_deref(),
            },
        )?;
        drop(progress);
        tracing::debug!(
            path = %self.root.display(),
            sending,
            planning,
            sent_bytes = report.sent_bytes,
            received_bytes = report.received_bytes,
            "Immutable history file transfer completed"
        );
        if !report.stderr.is_empty() {
            writeln!(ui.stderr(), "{}", report.stderr)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn transfer_git(
        &self,
        ui: &Ui,
        stream: &mut impl Stream,
        repository: &Path,
        sending: bool,
        receive_budget: u64,
    ) -> Result<(), CommandError> {
        self.progress.phase(
            ui,
            if sending {
                "Sending Git objects"
            } else {
                "Receiving Git objects"
            },
        )?;
        tracing::debug!(
            path = %self.root.display(),
            sending,
            "Transferring Git objects with native Git"
        );
        let backend = jj_lib::git::get_git_backend(self.helper.repo().store())?;
        // Git's input-size setting protects receive-pack, while fetch needs
        // the same bound at the wire. Allow a fixed amount for ref negotiation;
        // physical pack and metadata budgets are checked again before install.
        let max_received_bytes = if sending {
            u64::MAX
        } else {
            receive_budget.saturating_add(16 * 1024 * 1024)
        };
        let progress = self.progress.transfer(ui, "Transferring Git objects");
        let report = stream
            .transfer_git(
                backend.git_executable_path(),
                repository,
                sending,
                ProcessTransferOptions {
                    max_received_bytes,
                    progress: progress.as_deref(),
                },
            )
            .map_err(|error| {
                if command_error_message(&error).contains("exceeds maximum allowed size") {
                    error.hinted(
                        "The receiver's sync.max-quarantine-bytes setting limits the incoming Git \
                         pack.",
                    )
                } else {
                    error
                }
            })?;
        drop(progress);
        tracing::debug!(
            path = %self.root.display(),
            sending,
            sent_bytes = report.sent_bytes,
            received_bytes = report.received_bytes,
            "Native Git object transfer completed"
        );
        if !report.stderr.is_empty() {
            writeln!(ui.stderr(), "{}", report.stderr)?;
        }
        Ok(())
    }

    fn known_cache_limit(&self) -> Result<usize, CommandError> {
        let limit = positive_count_setting(self.helper.settings(), "sync.max-records")?
            .unwrap_or(MAX_KNOWN_RECORD_IDS)
            .min(MAX_KNOWN_RECORD_IDS);
        #[cfg(feature = "test-fakes")]
        let limit = std::env::var("JJ_TEST_SYNC_KNOWN_RECORD_IDS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map_or(limit, |value| limit.min(value));
        Ok(limit)
    }

    fn quarantine_limits(&self) -> Result<QuarantineLimits, CommandError> {
        let mut limits = QuarantineLimits::for_directory(&self.helper.repo_path().join("sync"))
            .map_err(failed)?;
        if let Some(HumanByteSize(bytes)) = self
            .helper
            .settings()
            .get_value_with("sync.max-quarantine-bytes", TryInto::try_into)
            .optional()?
        {
            if bytes == 0 {
                return Err(user_error(
                    "sync.max-quarantine-bytes must be greater than zero",
                ));
            }
            limits.max_staged_bytes = bytes;
        }
        if let Some(count) = positive_count_setting(self.helper.settings(), "sync.max-records")? {
            limits.max_records = count;
        }
        if let Some(count) =
            positive_count_setting(self.helper.settings(), "sync.max-dependencies")?
        {
            limits.max_dependencies = count;
        }
        Ok(limits)
    }

    async fn select(&self, ui: &Ui, incoming: &Advertisement) -> Result<OperationId, CommandError> {
        self.progress.phase(ui, "Merging repository views")?;
        self.endpoint
            .reconcile_native(
                self.helper.repo(),
                &[parse_raw(&incoming.current_operation)?],
                &format!("{}-selected", self.transfer),
            )
            .await
            .map_err(failed)
    }

    fn selected_inventory(
        &self,
        ui: &Ui,
        selected: &OperationId,
    ) -> Result<Inventory, CommandError> {
        self.progress
            .phase(ui, "Collecting merged history objects")?;
        let mut roots = self.capture.raw_heads.clone();
        roots.push(selected.clone());
        roots.sort_unstable();
        roots.dedup();
        collect_inventory(&self.helper, &self.endpoint, &roots, &[], self.packed_git)
    }

    async fn apply(
        &mut self,
        ui: &Ui,
        command: &CommandHelper,
        selected: &OperationId,
        incoming: &Advertisement,
    ) -> Result<Applied, CommandError> {
        self.progress.phase(ui, "Applying synchronized history")?;
        tracing::debug!(
            path = %self.root.display(),
            "Applying synchronized history"
        );
        // Ordinary commands can have advanced the endpoint during the transfer.
        // Refresh its normal workspace helper, then preserve that newer work as
        // a local causal descendant containing the agreed frozen result.
        self.helper = load_helper(ui, command, &self.root, self.ignore_working_copy).await?;
        let refreshed_pin = format!("{}-capture-{}", self.transfer, self.pins.len());
        self.pins.push(refreshed_pin.clone());
        let current = capture_current(
            ui,
            command,
            &mut self.helper,
            &self.endpoint,
            &self.root,
            self.ignore_working_copy,
            &refreshed_pin,
        )
        .await?;
        if let Some(tx) = prepare_integration(self.helper.repo(), std::slice::from_ref(selected))
            .await
            .map_err(failed)?
        {
            let session_key = format!("{}-apply", self.transfer);
            // A separate per-session lease proves that another live command
            // still owns its Pending WAL, without holding state.lock while
            // native finalization acquires ordinary workspace/Git locks.
            self.publication_leases.push(
                self.endpoint
                    .acquire_session_lease(&session_key)
                    .map_err(failed)?,
            );
            let session = SyncSession {
                peer: incoming.peer.clone(),
                baseline_operation: current.current_raw.hex(),
                incoming_raw_heads: incoming
                    .raw_heads
                    .iter()
                    .cloned()
                    .chain([selected.hex()])
                    .collect(),
                incoming_shared_heads: BTreeSet::new(),
                applied_anchor: None,
                result_operation: None,
                phase: SessionPhase::Pending,
            };
            {
                let mut state = self.endpoint.state_store().lock().map_err(failed)?;
                state
                    .state_mut()
                    .begin_session(session_key.clone(), session)
                    .map_err(failed)?;
                state.save().map_err(failed)?;
            }
            self.helper
                .finish_sync_transaction(ui, tx, "synchronize repository", |state, operation| {
                    let session = state
                        .state_mut()
                        .sessions
                        .get_mut(&session_key)
                        .ok_or_else(|| {
                            user_error("Synchronization publication receipt disappeared")
                        })?;
                    session.result_operation = Some(operation.id().hex());
                    Ok(())
                })
                .await?;
            self.endpoint
                .recover_published(self.helper.repo())
                .await
                .map_err(failed)?;
        }
        // Native finalization may add a real shared effect. Normalize its actual
        // operation, rather than pretending that it still equals the cutoff.
        let finalized_pin = format!("{}-capture-{}", self.transfer, self.pins.len());
        self.pins.push(finalized_pin.clone());
        let final_capture = capture_current(
            ui,
            command,
            &mut self.helper,
            &self.endpoint,
            &self.root,
            self.ignore_working_copy,
            &finalized_pin,
        )
        .await?;
        Ok(Applied {
            operation: self.helper.repo().op_id().hex(),
            captured_operation: selected.hex(),
            current_operation: if native::frontier(
                self.helper.repo().op_store().as_ref(),
                std::slice::from_ref(selected),
            )
            .await
            .map_err(failed)?
                == native::frontier(
                    self.helper.repo().op_store().as_ref(),
                    &final_capture.raw_heads,
                )
                .await
                .map_err(failed)?
            {
                selected.hex()
            } else {
                final_capture.current_raw.hex()
            },
        })
    }
}

impl Drop for PreparedEndpoint {
    fn drop(&mut self) {
        for pin in &self.pins {
            let _release_result = self.endpoint.release_transfer(pin);
        }
    }
}

async fn load_helper(
    ui: &Ui,
    command: &CommandHelper,
    root: &Path,
    ignore: bool,
) -> Result<WorkspaceCommandHelper, CommandError> {
    let (helper, stats, _) = command.workspace_helper_for_sync(ui, root, ignore).await?;
    print_snapshot_stats(ui, &stats, helper.env().path_converter())?;
    Ok(helper)
}

async fn migrate_legacy(
    ui: &Ui,
    helper: &mut WorkspaceCommandHelper,
    endpoint: &Endpoint,
) -> Result<(), CommandError> {
    let upgrade = endpoint.begin_native_upgrade().map_err(failed)?;
    if !endpoint.state_store().read().map_err(failed)?.is_native() {
        endpoint
            .recover_published(helper.repo())
            .await
            .map_err(failed)?;
        retire_interrupted_publications(endpoint, helper.repo()).await?;
        // Keep the deployed v1 collector's roots valid until publication. A
        // crash before the final state/marker change can safely retry migration.
        let key = format!("migration-{:032x}", rand::random::<u128>());
        let capture = endpoint
            .capture(helper.repo(), &key)
            .await
            .map_err(failed)?;
        let prepared_key = format!("{key}-prepared");
        let (tx, migration_roots) = {
            let mut state = endpoint.state_store().lock().map_err(failed)?;
            let tx = jj_lib::sync::migrate::prepare_legacy_migration(
                helper.repo(),
                state.state(),
                endpoint.anchors(),
            )
            .await
            .map_err(failed)?;
            state
                .state_mut()
                .pin_transfer(
                    prepared_key.clone(),
                    jj_lib::sync::state::PeerState {
                        raw_heads: tx.parent_ops().iter().map(|op| op.id().hex()).collect(),
                        shared_heads: BTreeSet::new(),
                    },
                )
                .map_err(failed)?;
            state.save().map_err(failed)?;
            (tx, state.state().gc_roots())
        };
        let baseline = helper.repo().op_id().hex();
        let _lease = endpoint.acquire_session_lease(&key).map_err(failed)?;
        helper
            .finish_sync_transaction(
                ui,
                tx,
                "migrate synchronized history to ordinary operations",
                |state, operation| {
                    check_migration_roots(state.state(), &migration_roots, None)?;
                    state
                        .state_mut()
                        .begin_session(
                            key.clone(),
                            SyncSession {
                                peer: "native-format-migration".to_owned(),
                                baseline_operation: baseline.clone(),
                                incoming_raw_heads: BTreeSet::from([operation.id().hex()]),
                                incoming_shared_heads: BTreeSet::new(),
                                applied_anchor: Some(capture.current_shared.hex()),
                                result_operation: Some(operation.id().hex()),
                                phase: SessionPhase::Pending,
                            },
                        )
                        .map_err(failed)
                },
            )
            .await?;
        endpoint
            .recover_published(helper.repo())
            .await
            .map_err(failed)?;
        let mut state = endpoint.state_store().lock().map_err(failed)?;
        check_migration_roots(state.state(), &migration_roots, Some(helper.repo().op_id()))?;
        state.state_mut().finish_native_migration();
        // Failed attempts left converted roots pinned. The exclusive upgrade
        // guard excludes another live migration, and the published join now
        // retains every checked root from those attempts through ordinary parents.
        state
            .state_mut()
            .transfer_pins
            .retain(|key, _| !key.starts_with("migration-"));
        state.save().map_err(failed)?;
    }
    upgrade.finish().map_err(failed)
}

fn check_migration_roots(
    state: &jj_lib::sync::state::SyncState,
    prepared: &jj_lib::sync::state::SyncGcRoots,
    published: Option<&OperationId>,
) -> Result<(), CommandError> {
    let roots = state.gc_roots();
    if !roots.shared_events.is_subset(&prepared.shared_events)
        || roots.raw_operations.iter().any(|id| {
            !prepared.raw_operations.contains(id)
                && published.is_none_or(|operation| operation.hex() != *id)
        })
    {
        return Err(user_error(
            "Synchronization history changed during migration; retry the command",
        ));
    }
    Ok(())
}

async fn capture_current(
    ui: &Ui,
    command: &CommandHelper,
    helper: &mut WorkspaceCommandHelper,
    endpoint: &Endpoint,
    root: &Path,
    ignore: bool,
    transfer: &str,
) -> Result<NativeCapture, CommandError> {
    for _ in 0..8 {
        match endpoint.capture_native(helper.repo(), transfer).await {
            Ok(capture) => return Ok(capture),
            Err(EndpointError::StaleCapture) => {
                *helper = load_helper(ui, command, root, ignore).await?;
            }
            Err(err) => return Err(failed(err)),
        }
    }
    Err(user_error(
        "Repository kept changing during synchronization capture; retry the command",
    ))
}

fn objects<'a>(
    helper: &'a WorkspaceCommandHelper,
    endpoint: &'a Endpoint,
) -> Result<RepositoryObjects<'a>, CommandError> {
    let operations = helper
        .repo()
        .op_store()
        .downcast_ref::<SimpleOpStore>()
        .ok_or_else(|| user_error("Synchronization requires the built-in operation store"))?;
    let backend = jj_lib::git::get_git_backend(helper.repo().store())?;
    RepositoryObjects::new(operations, endpoint.anchors(), backend).map_err(failed)
}

fn collect_inventory(
    helper: &WorkspaceCommandHelper,
    endpoint: &Endpoint,
    raw_heads: &[OperationId],
    shared_heads: &[SharedEventId],
    packed_git: bool,
) -> Result<Inventory, CommandError> {
    tracing::debug!(
        path = %helper.workspace_root().display(),
        "Collecting synchronization object inventory"
    );
    let objects = objects(helper, endpoint)?;
    let inventory = if packed_git {
        objects.inventory_packed(raw_heads, shared_heads)
    } else {
        objects.inventory(raw_heads, shared_heads)
    }
    .map_err(failed)?;
    tracing::debug!(
        path = %helper.workspace_root().display(),
        objects = inventory.records.len(),
        "Synchronization object inventory collected"
    );
    Ok(inventory)
}

async fn retire_interrupted_publications(
    endpoint: &Endpoint,
    repo: &std::sync::Arc<jj_lib::repo::ReadonlyRepo>,
) -> Result<(), CommandError> {
    let sessions = endpoint.state_store().read().map_err(failed)?.sessions;
    for (key, session) in sessions {
        if session.phase != SessionPhase::Pending {
            continue;
        }
        // The endpoint checks the session lease without waiting, so another
        // command's live receipt cannot block its own network response.
        endpoint
            .retire_unpublished(repo, &key)
            .await
            .map_err(failed)?;
    }
    Ok(())
}

/// Runs the public command against a filesystem or SSH endpoint.
pub(crate) async fn synchronize(
    ui: &Ui,
    command: &CommandHelper,
    peer: &sync_transport::Endpoint,
    direction: Direction,
) -> Result<(), CommandError> {
    check_command_options(command)?;
    let progress = SyncProgress { label: "Local:" };
    progress.phase(ui, "Opening repository")?;
    let local_root = match command.workspace_loader() {
        Ok(loader) => loader.workspace_root().to_owned(),
        Err(err) => {
            let root = command
                .global_args()
                .repository
                .as_ref()
                .map_or_else(|| command.cwd().to_owned(), |path| command.cwd().join(path));
            if direction == Direction::Send || !workspace_is_missing(&root)? {
                return Err(err);
            }
            root
        }
    };
    let ignore = command.global_args().ignore_working_copy;
    tracing::debug!(
        path = %local_root.display(),
        peer_path = %String::from_utf8_lossy(&peer.repository_path()),
        ?direction,
        ignore_working_copy = ignore,
        "Starting repository synchronization"
    );
    let local_missing = workspace_is_missing(&local_root)?;
    if local_missing && direction == Direction::Send {
        return Err(user_error(
            "Synchronization source repository does not exist",
        ));
    }
    let profile = (!local_missing)
        .then(|| command.load_workspace_for_sync_profile(ui, &local_root))
        .transpose()?;
    if let sync_transport::Endpoint::Local(path) = peer
        && !workspace_is_missing(path)?
    {
        let peer_profile = command.load_workspace_for_sync_profile(ui, path)?;
        if let Some(profile) = &profile
            && dunce::canonicalize(profile.repo_path())?
                == dunce::canonicalize(peer_profile.repo_path())?
        {
            return Err(user_error(
                "Cannot synchronize a repository with its own shared store",
            ));
        }
    }
    let mut hello = match &profile {
        Some(profile) => profile_hello(profile, direction, peer.repository_path(), ignore)?,
        None => {
            let mut hello = format_hello(
                direction,
                peer.repository_path(),
                ignore,
                NEGOTIATE_OBJECT_HASH,
                &command.settings().get::<PathBuf>("git.executable-path")?,
            );
            hello
                .required_capabilities
                .push(INITIALIZE_RECEIVER_CAPABILITY.to_owned());
            hello
        }
    };
    drop(profile);
    writeln!(ui.status(), "Connecting to peer...")?;
    let mut transport = Transport::connect(peer, command.global_args().debug).map_err(failed)?;
    transport
        .send(&Message::Hello(hello.clone()))
        .map_err(failed)?;
    let (packed_git, adaptive_metadata, rsync_immutable) =
        match transport.receive().map_err(failed)? {
            Message::HelloReply(reply) => {
                tracing::debug!(
                    object_hash = %reply.object_hash,
                    "Peer accepted synchronization handshake"
                );
                if local_missing {
                    hello.object_hash.clone_from(&reply.object_hash);
                }
                hello.validate_peer(&reply).map_err(failed)?;
                (
                    use_native_git(&hello, &reply),
                    use_adaptive_metadata(&hello, &reply),
                    use_rsync_immutable(&hello, &reply),
                )
            }
            Message::Error(error) => {
                return Err(user_error(format!("Peer synchronization failed: {error}")));
            }
            _ => {
                return Err(user_error(
                    "Peer did not negotiate a synchronization session",
                ));
            }
        };
    if local_missing {
        progress.phase(ui, "Initializing repository")?;
        let object_hash = parse_object_hash(&hello.object_hash)?;
        crate::commands::init_colocated_for_sync(ui, command, &local_root, object_hash).await?;
    }
    let mut local = PreparedEndpoint::open(
        ui,
        command,
        &local_root,
        ignore,
        packed_git,
        rsync_immutable,
        progress,
    )
    .await?;
    hello
        .validate_peer(&local.hello(direction, peer.repository_path())?)
        .map_err(failed)?;
    let local_advertisement = local.advertisement()?;
    send_command(&mut transport, "capture", &local_advertisement)?;
    if !rsync_immutable {
        send_inventory(&mut transport, &local.inventory, packed_git)?;
    }
    writeln!(ui.status(), "Waiting for peer to prepare history...")?;
    let remote: Advertisement = receive_command(&mut transport, "capture")?;
    remote.validate()?;
    let remote_known = if rsync_immutable {
        BTreeSet::new()
    } else {
        receive_inventory(&mut transport, local.known_cache_limit()?)?
    };
    if adaptive_metadata {
        local.progress.phase(ui, "Comparing commit metadata")?;
        let manifest = objects(&local.helper, &local.endpoint)?
            .commit_metadata_inventory(&local.inventory)
            .map_err(failed)?;
        local.common_metadata = negotiate_metadata(
            &mut transport,
            &manifest,
            parse_object_hash(&hello.object_hash)?,
            true,
        )?;
    }
    #[cfg(feature = "test-fakes")]
    crate::cli_util::pause_for_test("JJ_TEST_SYNC_AFTER_CAPTURE_PAUSE_DIR");
    let local_applied = match direction {
        Direction::Receive | Direction::Bidirectional => {
            let incoming = local.receive_offer(ui, &mut transport, &remote.peer, Some(&remote))?;
            let selected = local.select(ui, &incoming).await?;
            if direction == Direction::Bidirectional {
                let inventory = local.selected_inventory(ui, &selected)?;
                local.send_offer(ui, &mut transport, &inventory, &selected, &remote_known)?;
                writeln!(
                    ui.status(),
                    "Waiting for peer to validate received history..."
                )?;
                let ready: String = receive_command(&mut transport, "ready")?;
                if ready != selected.hex() {
                    return Err(user_error("Peer changed the selected operation result"));
                }
            }
            let applied = local.apply(ui, command, &selected, &incoming).await?;
            if direction == Direction::Bidirectional {
                writeln!(
                    ui.status(),
                    "Waiting for peer to apply synchronized history..."
                )?;
                let remote_result = (|| {
                    send_command(&mut transport, "apply", &selected.hex())?;
                    receive_command::<Applied>(&mut transport, "result")
                })();
                let remote_applied = remote_result.map_err(|err| {
                    err.hinted(
                        "The local repository has applied the captured history. The peer may also \
                         have published it; retrying checks durable history and completes the \
                         exchange. Undo remains local to each receiver.",
                    )
                })?;
                check_applied(&remote_applied, Some(&selected))?;
                print_applied(ui, "Peer", &remote_applied)?;
            }
            print_applied(ui, "Local repository", &applied)?;
            Some(applied)
        }
        Direction::Send => {
            local.send_offer(
                ui,
                &mut transport,
                &local.inventory,
                &local.capture.raw_heads[0],
                &remote_known,
            )?;
            writeln!(
                ui.status(),
                "Waiting for peer to apply synchronized history..."
            )?;
            let applied: Applied = receive_command(&mut transport, "result").map_err(|err| {
                err.hinted(
                    "The peer may already have published the synchronization. Retrying checks its \
                     durable history; a missing acknowledgment does not roll it back.",
                )
            })?;
            check_applied(&applied, None)?;
            print_applied(ui, "Peer", &applied)?;
            None
        }
    };
    send_command(&mut transport, "complete", &local_applied)?;
    match Stream::receive(&mut transport)? {
        Message::Done(bytes) if bytes.is_empty() => {}
        _ => {
            return Err(user_error(
                "Peer did not acknowledge synchronization completion",
            ));
        }
    }
    transport.finish().map_err(failed)?;
    writeln!(ui.status(), "Synchronization complete.")?;
    Ok(())
}

/// Runs the hidden helper. Stdout belongs exclusively to framed protocol data.
pub(crate) async fn serve(ui: &Ui, command: &CommandHelper) -> Result<(), CommandError> {
    check_command_options(command)?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut session = Session::new(io::BufReader::new(stdin), stdout);
    let result = serve_session(ui, command, &mut session).await;
    if let Err(error) = &result {
        // A failed native phase may already have stopped reading its peer's
        // input. Keep the final error smaller than a pipe buffer so reporting
        // it cannot prevent this helper from closing the peer's output pipe.
        // The normal command error printer retains full diagnostics on stderr.
        let mut message = command_error_message(error);
        if message.len() > 2048 {
            let mut end = 2000;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push_str("\n[Full diagnostics are in helper stderr.]");
        }
        let _send_error_result = Session::send(&mut session, &Message::Error(message));
    }
    result
}

async fn serve_session(
    ui: &Ui,
    command: &CommandHelper,
    stream: &mut impl Stream,
) -> Result<(), CommandError> {
    let progress = SyncProgress { label: "Peer:" };
    let Message::Hello(request) = stream.receive()? else {
        return Err(user_error("Expected synchronization negotiation"));
    };
    let path = sync_transport::decode_repository_path(&request.repository_path).map_err(failed)?;
    tracing::debug!(
        path = %path.display(),
        direction = ?request.direction,
        "Opening peer synchronization repository"
    );
    let negotiate_hash = request.object_hash == NEGOTIATE_OBJECT_HASH;
    if negotiate_hash
        && (request.direction == Direction::Send
            || !request
                .required_capabilities
                .iter()
                .any(|capability| capability == INITIALIZE_RECEIVER_CAPABILITY))
    {
        return Err(user_error(
            "Only a missing receiver can negotiate its object hash",
        ));
    }
    if workspace_is_missing(&path)? {
        // Validate formats and capabilities before creating any destination data.
        let object_hash = validate_receiver_request(&request)?;
        tracing::debug!(
            path = %path.display(),
            %object_hash,
            "Initializing synchronization destination"
        );
        progress.phase(ui, "Initializing repository")?;
        crate::commands::init_colocated_for_sync(ui, command, &path, object_hash).await?;
    }
    let profile = command.load_workspace_for_sync_profile(ui, &path)?;
    let reply = profile_hello(
        &profile,
        request.direction,
        Vec::new(),
        request.ignore_working_copy,
    )?;
    drop(profile);
    if negotiate_hash {
        let mut negotiated = request.clone();
        negotiated.object_hash.clone_from(&reply.object_hash);
        reply.validate_peer(&negotiated).map_err(failed)?;
    } else {
        reply.validate_peer(&request).map_err(failed)?;
    }
    stream.send(&Message::HelloReply(reply.clone()))?;
    let packed_git = use_native_git(&request, &reply);
    let adaptive_metadata = use_adaptive_metadata(&request, &reply);
    let rsync_immutable = use_rsync_immutable(&request, &reply);
    let mut local = PreparedEndpoint::open(
        ui,
        command,
        &path,
        request.ignore_working_copy,
        packed_git,
        rsync_immutable,
        progress,
    )
    .await?;
    reply
        .validate_peer(&local.hello(request.direction, Vec::new())?)
        .map_err(failed)?;
    let peer: Advertisement = receive_command(stream, "capture")?;
    peer.validate()?;
    let peer_known = if rsync_immutable {
        BTreeSet::new()
    } else {
        receive_inventory(stream, local.known_cache_limit()?)?
    };
    let advertisement = local.advertisement()?;
    send_command(stream, "capture", &advertisement)?;
    if !rsync_immutable {
        send_inventory(stream, &local.inventory, packed_git)?;
    }
    if adaptive_metadata {
        local.progress.phase(ui, "Comparing commit metadata")?;
        let manifest = objects(&local.helper, &local.endpoint)?
            .commit_metadata_inventory(&local.inventory)
            .map_err(failed)?;
        local.common_metadata = negotiate_metadata(
            stream,
            &manifest,
            parse_object_hash(&reply.object_hash)?,
            false,
        )?;
    }
    match request.direction {
        Direction::Receive => {
            local.send_offer(
                ui,
                stream,
                &local.inventory,
                &local.capture.raw_heads[0],
                &peer_known,
            )?;
        }
        Direction::Send => {
            let incoming = local.receive_offer(ui, stream, &peer.peer, Some(&peer))?;
            let selected = local.select(ui, &incoming).await?;
            let applied = local.apply(ui, command, &selected, &incoming).await?;
            send_command(stream, "result", &applied)?;
        }
        Direction::Bidirectional => {
            local.send_offer(
                ui,
                stream,
                &local.inventory,
                &local.capture.raw_heads[0],
                &peer_known,
            )?;
            let incoming = local.receive_offer(ui, stream, &peer.peer, None)?;
            let selected = parse_raw(&incoming.current_operation)?;
            let selected_frontier = native::frontier(
                local.helper.repo().op_store().as_ref(),
                std::slice::from_ref(&selected),
            )
            .await
            .map_err(failed)?;
            let combined_frontier = native::frontier(
                local.helper.repo().op_store().as_ref(),
                &[
                    selected.clone(),
                    local.capture.current_raw.clone(),
                    parse_raw(&peer.current_operation)?,
                ],
            )
            .await
            .map_err(failed)?;
            if selected_frontier != combined_frontier {
                return Err(user_error(
                    "Selected operation result does not include both captured histories",
                ));
            }
            send_command(stream, "ready", &selected.hex())?;
            let requested: String = receive_command(stream, "apply")?;
            if requested != selected.hex() {
                return Err(user_error(
                    "Initiator changed the selected operation result",
                ));
            }
            let applied = local.apply(ui, command, &selected, &incoming).await?;
            send_command(stream, "result", &applied)?;
        }
    }
    let _completed: Option<Applied> = receive_command(stream, "complete")?;
    stream.send(&Message::Done(Vec::new()))
}

fn check_command_options(command: &CommandHelper) -> Result<(), CommandError> {
    if command.global_args().at_operation.is_some() {
        return Err(user_error(
            "Synchronization uses current operation heads; remove --at-operation",
        ));
    }
    if !command.should_commit_transaction() {
        return Err(user_error(
            "Synchronization publishes receiver operations; remove --no-integrate-operation",
        ));
    }
    Ok(())
}

fn profile_hello(
    workspace: &Workspace,
    direction: Direction,
    path: Vec<u8>,
    ignore: bool,
) -> Result<Hello, CommandError> {
    let loader = workspace.repo_loader();
    let backend = jj_lib::git::get_git_backend(loader.store())?;
    if !matches!(
        loader.op_store().name(),
        SimpleOpStore::NAME | SimpleOpStore::SYNC_NAME | SimpleOpStore::NATIVE_SYNC_NAME
    ) {
        return Err(user_error(
            "Repository synchronization requires the built-in operation store",
        ));
    }
    let hash = match loader.store().root_commit_id().as_bytes().len() {
        20 => "sha1",
        32 => "sha256",
        _ => {
            return Err(user_error(
                "Unsupported Git object hash for synchronization",
            ));
        }
    };
    Ok(format_hello(
        direction,
        path,
        ignore,
        hash,
        backend.git_executable_path(),
    ))
}

fn format_hello(
    direction: Direction,
    path: Vec<u8>,
    ignore: bool,
    hash: &str,
    git: &Path,
) -> Hello {
    let native_git = cfg!(unix)
        && std::process::Command::new(git)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
    #[cfg(feature = "test-fakes")]
    let native_git = native_git
        && std::env::var("JJ_TEST_SYNC_DISABLE_NATIVE_GIT").map_or(true, |disabled| {
            disabled != "all" && !(disabled == "helper" && path.is_empty())
        });
    let adaptive_metadata = native_git;
    #[cfg(feature = "test-fakes")]
    let adaptive_metadata = adaptive_metadata
        && std::env::var("JJ_TEST_SYNC_DISABLE_METADATA_NEGOTIATION").map_or(true, |disabled| {
            disabled != "all" && !(disabled == "helper" && path.is_empty())
        });
    #[cfg(unix)]
    let rsync_immutable = native_git && crate::sync_rsync_transport::available(Path::new("rsync"));
    #[cfg(not(unix))]
    let rsync_immutable = false;
    #[cfg(feature = "test-fakes")]
    let rsync_immutable = rsync_immutable
        && std::env::var("JJ_TEST_SYNC_DISABLE_RSYNC").map_or(true, |disabled| {
            disabled != "all" && !(disabled == "helper" && path.is_empty())
        });
    Hello {
        version: PROTOCOL_VERSION,
        direction,
        repository_path: path,
        ignore_working_copy: ignore,
        backend: "git".to_owned(),
        object_hash: hash.to_owned(),
        operation_store: "simple-op-v1".to_owned(),
        capabilities: CAPABILITIES
            .iter()
            .copied()
            .chain([INITIALIZE_RECEIVER_CAPABILITY])
            .chain(native_git.then_some(NATIVE_GIT_CAPABILITY))
            .chain(adaptive_metadata.then_some(ADAPTIVE_METADATA_CAPABILITY))
            .chain(rsync_immutable.then_some(RSYNC_IMMUTABLE_CAPABILITY))
            .map(str::to_owned)
            .collect(),
        required_capabilities: CAPABILITIES.iter().map(|name| (*name).to_owned()).collect(),
    }
}

fn use_native_git(request: &Hello, reply: &Hello) -> bool {
    cfg!(unix)
        && [request, reply].iter().all(|hello| {
            hello
                .capabilities
                .iter()
                .any(|capability| capability == NATIVE_GIT_CAPABILITY)
        })
}

fn use_adaptive_metadata(request: &Hello, reply: &Hello) -> bool {
    use_native_git(request, reply)
        && [request, reply].iter().all(|hello| {
            hello
                .capabilities
                .iter()
                .any(|capability| capability == ADAPTIVE_METADATA_CAPABILITY)
        })
}

fn use_rsync_immutable(request: &Hello, reply: &Hello) -> bool {
    use_native_git(request, reply)
        && [request, reply].iter().all(|hello| {
            hello
                .capabilities
                .iter()
                .any(|capability| capability == RSYNC_IMMUTABLE_CAPABILITY)
        })
}

/// An ordered prefix frontier gives both peers the same requests and bounds
/// traversal by the locally captured keys, even if peer counts are dishonest.
struct MetadataProbe {
    prefix: String,
    peer_parent_count: usize,
}

struct MetadataNegotiation<'a> {
    inventory: &'a MetadataInventory,
    hash: gix::hash::Kind,
    pending: VecDeque<MetadataProbe>,
    known: KnownCommitMetadata,
    visited: usize,
    limit: usize,
}

impl<'a> MetadataNegotiation<'a> {
    fn new(inventory: &'a MetadataInventory, hash: gix::hash::Kind) -> Self {
        Self {
            inventory,
            hash,
            pending: VecDeque::from([MetadataProbe {
                prefix: String::new(),
                peer_parent_count: usize::MAX,
            }]),
            known: KnownCommitMetadata::new(),
            visited: 0,
            limit: inventory
                .len()
                .saturating_mul(hash.len_in_bytes() * 2)
                .saturating_mul(16)
                .saturating_add(1),
        }
    }

    fn batch(&mut self) -> Result<Vec<MetadataProbe>, CommandError> {
        let count = self.pending.len().min(METADATA_PROBE_BATCH_SIZE);
        self.visited = self
            .visited
            .checked_add(count)
            .ok_or_else(|| user_error("Commit metadata negotiation exceeded its probe limit"))?;
        if self.visited > self.limit {
            return Err(user_error(
                "Commit metadata negotiation exceeded its probe limit",
            ));
        }
        Ok(self.pending.drain(..count).collect())
    }

    fn summaries(&self, probes: &[MetadataProbe]) -> Result<Vec<MetadataSummary>, CommandError> {
        probes
            .iter()
            .map(|probe| self.inventory.summarize(&probe.prefix).map_err(failed))
            .collect()
    }

    fn accept(
        &mut self,
        probes: &[MetadataProbe],
        local: &[MetadataSummary],
        peer: &[MetadataSummary],
    ) -> Result<(), CommandError> {
        if peer.len() != probes.len() || peer.len() > METADATA_PROBE_BATCH_SIZE {
            return Err(user_error("Invalid commit metadata probe batch size"));
        }
        // Apart from the initial root, requests contain complete ordered sibling
        // groups. Child counts must partition their advertised parent count.
        if !probes[0].prefix.is_empty() {
            for (siblings, summaries) in probes.chunks(16).zip(peer.chunks(16)) {
                let count = summaries
                    .iter()
                    .try_fold(0usize, |sum, summary| sum.checked_add(summary.count));
                if siblings.len() != 16 || count != Some(siblings[0].peer_parent_count) {
                    return Err(user_error("Inconsistent commit metadata subtree counts"));
                }
            }
        }
        for ((probe, local), peer) in probes.iter().zip(local).zip(peer) {
            peer.validate(self.hash).map_err(failed)?;
            let capacity =
                16usize.saturating_pow((self.hash.len_in_bytes() * 2 - probe.prefix.len()) as u32);
            if peer.prefix != probe.prefix
                || peer.count > probe.peer_parent_count
                || peer.count > capacity
            {
                return Err(user_error("Noncanonical commit metadata probe sequence"));
            }
            if local.count == 0 || peer.count == 0 {
                continue;
            }
            if local.count == peer.count && local.digest == peer.digest {
                self.inventory
                    .append_matching(&probe.prefix, &mut self.known)
                    .map_err(failed)?;
            } else if (local.count > 1 || peer.count > 1)
                && probe.prefix.len() < self.hash.len_in_bytes() * 2
            {
                for digit in b"0123456789abcdef" {
                    self.pending.push_back(MetadataProbe {
                        prefix: format!("{}{}", probe.prefix, char::from(*digit)),
                        peer_parent_count: peer.count,
                    });
                }
            }
        }
        Ok(())
    }
}

fn negotiate_metadata(
    stream: &mut impl Stream,
    inventory: &MetadataInventory,
    hash: gix::hash::Kind,
    client: bool,
) -> Result<KnownCommitMetadata, CommandError> {
    let mut negotiation = MetadataNegotiation::new(inventory, hash);
    while !negotiation.pending.is_empty() {
        let probes = negotiation.batch()?;
        let local = negotiation.summaries(&probes)?;
        let peer = if client {
            send_command(stream, "metadata-probe", &local)?;
            receive_command::<Vec<MetadataSummary>>(stream, "metadata-reply")?
        } else {
            receive_command::<Vec<MetadataSummary>>(stream, "metadata-probe")?
        };
        negotiation.accept(&probes, &local, &peer)?;
        if !client {
            send_command(stream, "metadata-reply", &local)?;
        }
    }
    tracing::debug!(
        matching_commits = negotiation.known.len(),
        probes = negotiation.visited,
        "Commit metadata negotiation completed"
    );
    Ok(negotiation.known)
}

/// An invalid marker is an existing broken repository, never an init request.
fn workspace_is_missing(path: &Path) -> Result<bool, CommandError> {
    match std::fs::symlink_metadata(path.join(".jj")) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(user_error_with_message(
            format!(
                "Failed to inspect synchronization endpoint {}",
                path.display()
            ),
            err,
        )),
    }
}

fn validate_receiver_request(request: &Hello) -> Result<gix::hash::Kind, CommandError> {
    if request.direction == Direction::Receive || request.object_hash == NEGOTIATE_OBJECT_HASH {
        return Err(user_error(
            "Synchronization source repository does not exist",
        ));
    }
    let expected = format_hello(
        request.direction,
        Vec::new(),
        request.ignore_working_copy,
        &request.object_hash,
        Path::new("git"),
    );
    expected.validate_peer(request).map_err(failed)?;
    parse_object_hash(&request.object_hash)
}

fn parse_object_hash(hash: &str) -> Result<gix::hash::Kind, CommandError> {
    match hash {
        "sha1" => Ok(gix::hash::Kind::Sha1),
        "sha256" => Ok(gix::hash::Kind::Sha256),
        _ => Err(user_error(
            "Unsupported Git object hash for synchronization",
        )),
    }
}

fn send_inventory(
    stream: &mut impl Stream,
    inventory: &Inventory,
    packed_git: bool,
) -> Result<(), CommandError> {
    let mut batch = Vec::with_capacity(INVENTORY_BATCH_SIZE);
    for record in &inventory.records {
        if packed_git && matches!(record, RecordId::Git(_)) {
            continue;
        }
        batch.push(WireRecordId::from(record));
        if batch.len() == INVENTORY_BATCH_SIZE {
            send_command(stream, "inventory", &batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        send_command(stream, "inventory", &batch)?;
    }
    stream.send(&Message::Done(Vec::new()))
}

fn receive_inventory(
    stream: &mut impl Stream,
    cache_limit: usize,
) -> Result<BTreeSet<RecordId>, CommandError> {
    let mut records = BTreeSet::new();
    let mut previous = None;
    loop {
        match stream.receive()? {
            Message::Command { name, payload } if name == "inventory" => {
                if payload.len() > MAX_INVENTORY_BATCH_BYTES {
                    return Err(user_error(
                        "Synchronization inventory batch exceeds its byte limit",
                    ));
                }
                let batch: Vec<WireRecordId> = serde_json::from_slice(&payload).map_err(failed)?;
                if batch.is_empty() || batch.len() > INVENTORY_BATCH_SIZE {
                    return Err(user_error("Invalid synchronization inventory batch length"));
                }
                for record in batch {
                    let id = record.decode()?;
                    if previous.as_ref().is_some_and(|previous| previous >= &id) {
                        return Err(user_error(
                            "Synchronization inventory is not canonically ordered",
                        ));
                    }
                    previous = Some(id.clone());
                    if records.len() < cache_limit.min(MAX_KNOWN_RECORD_IDS) {
                        records.insert(id);
                    }
                }
            }
            Message::Done(bytes) if bytes.is_empty() => return Ok(records),
            _ => return Err(user_error("Unexpected synchronization inventory message")),
        }
    }
}

fn positive_count_setting(
    settings: &jj_lib::settings::UserSettings,
    key: &'static str,
) -> Result<Option<usize>, CommandError> {
    let value = settings.get::<usize>(key).optional()?;
    if value == Some(0) {
        return Err(user_error(format!("{key} must be greater than zero")));
    }
    Ok(value)
}

fn send_command(
    stream: &mut impl Stream,
    name: &str,
    payload: &impl Serialize,
) -> Result<(), CommandError> {
    stream.send(&Message::Command {
        name: name.to_owned(),
        payload: serde_json::to_vec(payload).map_err(failed)?,
    })
}

fn receive_command<T: for<'de> Deserialize<'de>>(
    stream: &mut impl Stream,
    expected: &str,
) -> Result<T, CommandError> {
    match stream.receive()? {
        Message::Command { name, payload } if name == expected => {
            if payload.len() > MAX_PHASE_CONTROL_BYTES {
                return Err(user_error(
                    "Synchronization phase control payload exceeds its byte limit",
                ));
            }
            serde_json::from_slice(&payload).map_err(failed)
        }
        _ => Err(user_error(format!(
            "Expected synchronization phase {expected:?}"
        ))),
    }
}

fn print_applied(ui: &Ui, endpoint: &str, result: &Applied) -> Result<(), CommandError> {
    if result.current_operation != result.captured_operation {
        writeln!(
            ui.status(),
            "{endpoint} applied the captured history; newer local changes remain for the next \
             synchronization."
        )?;
    }
    Ok(())
}

fn check_applied(result: &Applied, selected: Option<&OperationId>) -> Result<(), CommandError> {
    parse_raw(&result.operation)?;
    parse_raw(&result.captured_operation)?;
    parse_raw(&result.current_operation)?;
    if selected.is_some_and(|id| result.captured_operation != id.hex()) {
        return Err(user_error(
            "Peer reported a different applied synchronization result",
        ));
    }
    Ok(())
}

fn check_captured_offer(
    offered: &Advertisement,
    captured: &Advertisement,
) -> Result<(), CommandError> {
    if offered.peer != captured.peer
        || offered.raw_heads != captured.raw_heads
        || offered.current_operation != captured.current_operation
    {
        return Err(user_error(
            "Peer changed its frozen synchronization cutoff after advertising it",
        ));
    }
    Ok(())
}

fn check_id(value: &str) -> Result<(), CommandError> {
    if value.len() != 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(user_error(
            "Invalid complete operation or shared-event identity from peer",
        ));
    }
    Ok(())
}

fn parse_raw(value: &str) -> Result<OperationId, CommandError> {
    check_id(value)?;
    Ok(OperationId::try_from_hex(value).unwrap())
}

fn parse_shared(value: &str) -> Result<SharedEventId, CommandError> {
    check_id(value)?;
    Ok(SharedEventId::try_from_hex(value).unwrap())
}

fn failed(error: impl std::error::Error + Send + Sync + 'static) -> CommandError {
    user_error_with_message("Repository synchronization failed", error)
}

fn command_error_message(error: &CommandError) -> String {
    let mut message = error.error.to_string();
    let mut source = error.error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_negotiation_keeps_common_sidecars_and_excludes_changed_identities() {
        for hash in [gix::hash::Kind::Sha1, gix::hash::Kind::Sha256] {
            let mut source = MetadataInventory::new(hash);
            let mut peer = MetadataInventory::new(hash);
            let mut expected = KnownCommitMetadata::new();
            for index in 0..64u8 {
                let id = gix::ObjectId::from_bytes_or_panic(&vec![index; hash.len_in_bytes()]);
                source.insert(id, [index; 32]).unwrap();
                if index == 17 {
                    peer.insert(id, [255; 32]).unwrap();
                } else {
                    peer.insert(id, [index; 32]).unwrap();
                    expected.insert(id, [index; 32]);
                }
            }
            peer.insert(
                gix::ObjectId::from_bytes_or_panic(&vec![255; hash.len_in_bytes()]),
                [255; 32],
            )
            .unwrap();
            let mut client = MetadataNegotiation::new(&source, hash);
            let mut server = MetadataNegotiation::new(&peer, hash);
            while !client.pending.is_empty() {
                let client_probes = client.batch().unwrap();
                let server_probes = server.batch().unwrap();
                assert_eq!(
                    client_probes
                        .iter()
                        .map(|probe| &probe.prefix)
                        .collect::<Vec<_>>(),
                    server_probes
                        .iter()
                        .map(|probe| &probe.prefix)
                        .collect::<Vec<_>>(),
                );
                let left = client.summaries(&client_probes).unwrap();
                let right = server.summaries(&server_probes).unwrap();
                client.accept(&client_probes, &left, &right).unwrap();
                server.accept(&server_probes, &right, &left).unwrap();
            }
            assert!(server.pending.is_empty());
            assert_eq!(client.known, expected);
            assert_eq!(server.known, expected);
            assert!(client.visited < source.len() * 2);
        }
    }

    #[test]
    fn metadata_negotiation_never_descends_an_empty_local_inventory() {
        let inventory = MetadataInventory::new(gix::hash::Kind::Sha1);
        let mut negotiation = MetadataNegotiation::new(&inventory, gix::hash::Kind::Sha1);
        let probes = negotiation.batch().unwrap();
        let local = negotiation.summaries(&probes).unwrap();
        let peer = [MetadataSummary {
            prefix: String::new(),
            count: usize::MAX,
            digest: "00".repeat(32),
        }];
        negotiation.accept(&probes, &local, &peer).unwrap();
        assert!(negotiation.pending.is_empty());
        assert!(negotiation.known.is_empty());
        assert_eq!(negotiation.visited, 1);
    }

    #[test]
    fn metadata_negotiation_rejects_noncanonical_batches_and_counts() {
        let hash = gix::hash::Kind::Sha1;
        let mut inventory = MetadataInventory::new(hash);
        inventory
            .insert(gix::ObjectId::from_bytes_or_panic(&[1; 20]), [1; 32])
            .unwrap();
        inventory
            .insert(gix::ObjectId::from_bytes_or_panic(&[2; 20]), [2; 32])
            .unwrap();
        for change in 0..4 {
            let mut negotiation = MetadataNegotiation::new(&inventory, hash);
            let probes = negotiation.batch().unwrap();
            let local = negotiation.summaries(&probes).unwrap();
            let mut peer = local.clone();
            match change {
                0 => peer.clear(),
                1 => peer.push(peer[0].clone()),
                2 => peer[0].prefix = "0".to_owned(),
                3 => peer[0].digest = "G".repeat(64),
                _ => unreachable!(),
            }
            assert!(negotiation.accept(&probes, &local, &peer).is_err());
        }
        let mut negotiation = MetadataNegotiation::new(&inventory, hash);
        let probes = negotiation.batch().unwrap();
        let local = negotiation.summaries(&probes).unwrap();
        let mut peer = local.clone();
        peer[0].digest = "00".repeat(32);
        peer[0].count = usize::MAX;
        negotiation.accept(&probes, &local, &peer).unwrap();
        let probes = negotiation.batch().unwrap();
        let local = negotiation.summaries(&probes).unwrap();
        let mut peer = local.clone();
        peer[0].count = usize::MAX;
        peer[1].count = 1;
        assert!(negotiation.accept(&probes, &local, &peer).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn adaptive_metadata_requires_native_git_and_both_optional_capabilities() {
        let mut hello = format_hello(Direction::Send, vec![1], false, "sha1", Path::new("git"));
        hello.capabilities.extend([
            NATIVE_GIT_CAPABILITY.to_owned(),
            ADAPTIVE_METADATA_CAPABILITY.to_owned(),
        ]);
        assert!(use_adaptive_metadata(&hello, &hello));
        assert!(
            !hello
                .required_capabilities
                .iter()
                .any(|capability| capability == ADAPTIVE_METADATA_CAPABILITY)
        );
        for removed in [NATIVE_GIT_CAPABILITY, ADAPTIVE_METADATA_CAPABILITY] {
            let mut old = hello.clone();
            old.capabilities.retain(|capability| capability != removed);
            assert!(!use_adaptive_metadata(&hello, &old));
            assert!(!use_adaptive_metadata(&old, &hello));
        }
    }

    #[cfg(unix)]
    #[test]
    fn immutable_files_require_native_git_and_both_optional_capabilities() {
        let mut hello = format_hello(Direction::Send, vec![1], false, "sha1", Path::new("git"));
        for capability in [NATIVE_GIT_CAPABILITY, RSYNC_IMMUTABLE_CAPABILITY] {
            if !hello.capabilities.iter().any(|value| value == capability) {
                hello.capabilities.push(capability.to_owned());
            }
        }
        assert!(use_rsync_immutable(&hello, &hello));
        assert!(
            !hello
                .required_capabilities
                .iter()
                .any(|capability| capability == RSYNC_IMMUTABLE_CAPABILITY)
        );
        for removed in [NATIVE_GIT_CAPABILITY, RSYNC_IMMUTABLE_CAPABILITY] {
            let mut old = hello.clone();
            old.capabilities.retain(|capability| capability != removed);
            assert!(!use_rsync_immutable(&hello, &old));
            assert!(!use_rsync_immutable(&old, &hello));
        }
    }

    #[test]
    fn new_receiver_rejects_incompatible_negotiation_before_initialization() {
        let valid = format_hello(
            Direction::Send,
            b"/missing".to_vec(),
            false,
            "sha1",
            Path::new("git"),
        );
        assert_eq!(
            validate_receiver_request(&valid).unwrap(),
            gix::hash::Kind::Sha1
        );
        let sha256 = Hello {
            object_hash: "sha256".to_owned(),
            ..valid.clone()
        };
        assert_eq!(
            validate_receiver_request(&sha256).unwrap(),
            gix::hash::Kind::Sha256
        );
        for field in 0..8 {
            let mut request = valid.clone();
            match field {
                0 => request.version += 1,
                1 => request.backend = "other".to_owned(),
                2 => request.operation_store = "other".to_owned(),
                3 => request.object_hash = "sha512".to_owned(),
                4 => request
                    .required_capabilities
                    .push("unknown-required".to_owned()),
                5 => request.capabilities.clear(),
                6 => request.direction = Direction::Receive,
                7 => request.object_hash = NEGOTIATE_OBJECT_HASH.to_owned(),
                _ => unreachable!(),
            }
            assert!(
                validate_receiver_request(&request).is_err(),
                "accepted field {field}"
            );
        }
    }

    #[test]
    fn incoming_ids_are_full_lowercase_and_typed() {
        for id in ["", "00", &"A".repeat(128), &"g".repeat(128)] {
            assert!(parse_raw(id).is_err());
            assert!(parse_shared(id).is_err());
        }
        let id = "01".repeat(64);
        assert_eq!(parse_raw(&id).unwrap().hex(), id);
        assert!(WireRecordId::Git("a".repeat(39)).decode().is_err());
    }

    #[test]
    fn inventories_cross_bounded_control_frames() {
        let records = (0..INVENTORY_BATCH_SIZE + 7)
            .map(|number| {
                let mut bytes = [0; 64];
                bytes[56..].copy_from_slice(&(number as u64).to_be_bytes());
                RecordId::Operation(OperationId::from_bytes(&bytes))
            })
            .collect::<BTreeSet<_>>();
        let inventory = Inventory {
            raw_heads: BTreeSet::new(),
            shared_heads: BTreeSet::new(),
            git_roots: BTreeSet::new(),
            records: records.clone(),
            git_commits: BTreeSet::new(),
            legacy_tree_roots: BTreeSet::new(),
        };
        let mut output = Session::new(io::empty(), Vec::new());
        send_inventory(&mut output, &inventory, false).unwrap();
        let bytes = output.into_inner().1;
        let mut input = Session::new(io::Cursor::new(bytes), io::sink());
        assert_eq!(
            receive_inventory(&mut input, MAX_KNOWN_RECORD_IDS).unwrap(),
            records
        );
    }

    #[test]
    fn inventories_reject_oversized_batches_and_duplicate_ids() {
        let record = WireRecordId::Operation("00".repeat(64));
        for batch in [
            vec![record.clone(); INVENTORY_BATCH_SIZE + 1],
            vec![record.clone(), record],
        ] {
            let mut output = Session::new(io::empty(), Vec::new());
            send_command(&mut output, "inventory", &batch).unwrap();
            let mut input = Session::new(io::Cursor::new(output.into_inner().1), io::sink());
            assert!(receive_inventory(&mut input, MAX_KNOWN_RECORD_IDS).is_err());
        }
    }

    #[test]
    fn large_peer_inventory_uses_bounded_cache_and_resends_omitted_records() {
        struct InventorySource {
            next: usize,
            total: usize,
        }
        impl Stream for InventorySource {
            fn send(&mut self, _message: &Message) -> Result<(), CommandError> {
                unreachable!()
            }

            fn receive(&mut self) -> Result<Message, CommandError> {
                if self.next == self.total {
                    return Ok(Message::Done(Vec::new()));
                }
                let end = self.total.min(self.next + INVENTORY_BATCH_SIZE);
                let batch = (self.next..end)
                    .map(|number| WireRecordId::from(&inventory_record(number)))
                    .collect::<Vec<_>>();
                self.next = end;
                Ok(Message::Command {
                    name: "inventory".to_owned(),
                    payload: serde_json::to_vec(&batch).unwrap(),
                })
            }
        }

        fn inventory_record(number: usize) -> RecordId {
            let mut bytes = [0; 64];
            bytes[56..].copy_from_slice(&(number as u64).to_be_bytes());
            RecordId::Operation(OperationId::from_bytes(&bytes))
        }

        let mut stream = InventorySource {
            next: 0,
            total: MAX_KNOWN_RECORD_IDS + INVENTORY_BATCH_SIZE + 1,
        };
        let known = receive_inventory(&mut stream, MAX_KNOWN_RECORD_IDS).unwrap();
        assert_eq!(known.len(), MAX_KNOWN_RECORD_IDS);
        assert_eq!(stream.next, stream.total);
        let omitted = inventory_record(MAX_KNOWN_RECORD_IDS);
        assert!(!known.contains(&omitted));
        let local = Inventory {
            raw_heads: BTreeSet::new(),
            shared_heads: BTreeSet::new(),
            git_roots: BTreeSet::new(),
            records: BTreeSet::from([inventory_record(0), omitted.clone()]),
            git_commits: BTreeSet::new(),
            legacy_tree_roots: BTreeSet::new(),
        };
        assert_eq!(
            local.missing(&known).cloned().collect::<Vec<_>>(),
            vec![omitted]
        );
    }

    #[test]
    fn inventory_rejects_oversized_json_before_deserialization() {
        let mut output = Session::new(io::empty(), Vec::new());
        Stream::send(
            &mut output,
            &Message::Command {
                name: "inventory".to_owned(),
                payload: vec![b' '; MAX_INVENTORY_BATCH_BYTES + 1],
            },
        )
        .unwrap();
        let mut input = Session::new(io::Cursor::new(output.into_inner().1), io::sink());
        let error = receive_inventory(&mut input, MAX_KNOWN_RECORD_IDS).unwrap_err();
        assert!(error.error.to_string().contains("byte limit"));
    }

    #[test]
    fn phase_controls_bound_allocating_json_before_deserialization() {
        let advertised = Advertisement {
            peer: "peer".to_owned(),
            raw_heads: vec![String::new(); MAX_PHASE_CONTROL_BYTES / 3],
            current_operation: "01".repeat(64),
        };
        assert!(serde_json::to_vec(&advertised).unwrap().len() > MAX_PHASE_CONTROL_BYTES);
        for phase in ["capture", "offer"] {
            let mut output = Session::new(io::empty(), Vec::new());
            send_command(&mut output, phase, &advertised).unwrap();
            let mut input = Session::new(io::Cursor::new(output.into_inner().1), io::sink());
            let error = receive_command::<Advertisement>(&mut input, phase).unwrap_err();
            assert!(error.error.to_string().contains("control payload exceeds"));
        }

        let advertised = Advertisement {
            raw_heads: vec!["01".repeat(64)],
            ..advertised
        };
        let mut output = Session::new(io::empty(), Vec::new());
        send_command(&mut output, "capture", &advertised).unwrap();
        let mut input = Session::new(io::Cursor::new(output.into_inner().1), io::sink());
        receive_command::<Advertisement>(&mut input, "capture")
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn phase_controls_bound_malformed_acknowledgments_before_deserialization() {
        for phase in [
            "ready",
            "apply",
            "result",
            "complete",
            "metadata-probe",
            "metadata-reply",
        ] {
            let mut output = Session::new(io::empty(), Vec::new());
            Stream::send(
                &mut output,
                &Message::Command {
                    name: phase.to_owned(),
                    payload: vec![b' '; MAX_PHASE_CONTROL_BYTES + 1],
                },
            )
            .unwrap();
            let mut input = Session::new(io::Cursor::new(output.into_inner().1), io::sink());
            let error = receive_command::<String>(&mut input, phase).unwrap_err();
            assert!(error.error.to_string().contains("control payload exceeds"));
        }
    }

    #[test]
    fn initial_offer_must_match_the_advertised_cutoff() {
        let captured = Advertisement {
            peer: "peer".to_owned(),
            raw_heads: vec!["01".repeat(64)],
            current_operation: "01".repeat(64),
        };
        check_captured_offer(&captured, &captured).unwrap();
        for field in 0..3 {
            let mut changed = captured.clone();
            match field {
                0 => changed.peer = "other".to_owned(),
                1 => changed.raw_heads = vec!["03".repeat(64)],
                2 => changed.current_operation = "05".repeat(64),
                _ => unreachable!(),
            }
            assert!(check_captured_offer(&changed, &captured).is_err());
        }
    }
}
