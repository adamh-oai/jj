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
use std::io;
use std::io::Read;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use jj_lib::config::ConfigGetResultExt as _;
use jj_lib::lock::FileLock;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::op_store::ViewId;
use jj_lib::repo::Repo as _;
use jj_lib::settings::HumanByteSize;
use jj_lib::simple_op_store::SimpleOpStore;
use jj_lib::sync::SyncError;
use jj_lib::sync::endpoint::Capture;
use jj_lib::sync::endpoint::Endpoint;
use jj_lib::sync::endpoint::EndpointError;
use jj_lib::sync::objects::Inventory;
use jj_lib::sync::objects::Quarantine;
use jj_lib::sync::objects::QuarantineLimits;
use jj_lib::sync::objects::Record;
use jj_lib::sync::objects::RecordId;
use jj_lib::sync::objects::RepositoryObjects;
use jj_lib::sync::protocol::Direction;
use jj_lib::sync::protocol::Hello;
use jj_lib::sync::protocol::Message;
use jj_lib::sync::protocol::PROTOCOL_VERSION;
use jj_lib::sync::protocol::Session;
use jj_lib::sync::reconcile::prepare_integration;
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
use crate::sync_transport;
use crate::sync_transport::Transport;
use crate::ui::Ui;

const CAPABILITIES: [&str; 4] = [
    "raw-history-v1",
    "shared-causal-v1",
    "streaming-objects-v1",
    "local-workspaces-v1",
];
const INITIALIZE_RECEIVER_CAPABILITY: &str = "initialize-receiver-v1";
const NEGOTIATE_OBJECT_HASH: &str = "auto";
const INVENTORY_BATCH_SIZE: usize = 512;
const MAX_INVENTORY_BATCH_BYTES: usize = INVENTORY_BATCH_SIZE * 192;
// Phase controls contain captured roots or publication acknowledgments. Object
// inventories and bodies use separate batches and streams, regardless of size.
const MAX_PHASE_CONTROL_BYTES: usize = 64 * 1024;
// Known IDs only avoid retransmission. Bound this peer-controlled cache and
// resend anything omitted from it, so large repositories remain supported.
const MAX_KNOWN_RECORD_IDS: usize = 65_536;

trait Stream {
    fn send(&mut self, message: &Message) -> Result<(), CommandError>;
    fn receive(&mut self) -> Result<Message, CommandError>;
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
}

impl<R: Read, W: io::Write> Stream for Session<R, W> {
    fn send(&mut self, message: &Message) -> Result<(), CommandError> {
        Self::send(self, message).map_err(failed)
    }

    fn receive(&mut self) -> Result<Message, CommandError> {
        Self::receive(self)
            .map_err(failed)
            .and_then(check_peer_error)
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
    shared_heads: Vec<String>,
    current_shared: String,
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

    fn shared_heads(&self) -> Result<Vec<SharedEventId>, CommandError> {
        if self.shared_heads.is_empty() {
            return Err(user_error("Peer advertised no shared operation heads"));
        }
        self.shared_heads
            .iter()
            .map(|id| parse_shared(id))
            .collect()
    }

    fn validate(&self) -> Result<(), CommandError> {
        if self.peer.is_empty() || self.peer.len() > 1024 {
            return Err(user_error("Peer advertised an invalid endpoint identity"));
        }
        self.raw_heads()?;
        let shared = self.shared_heads()?;
        if !shared.contains(&parse_shared(&self.current_shared)?) {
            return Err(user_error(
                "Peer's current shared operation is not a captured head",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Applied {
    operation: String,
    captured_anchor: String,
    current_anchor: String,
}

struct PreparedEndpoint {
    helper: WorkspaceCommandHelper,
    endpoint: Endpoint,
    capture: Capture,
    inventory: Inventory,
    root: PathBuf,
    ignore_working_copy: bool,
    transfer: String,
    pins: Vec<String>,
    publication_leases: Vec<FileLock>,
}

impl PreparedEndpoint {
    async fn open(
        ui: &Ui,
        command: &CommandHelper,
        root: &Path,
        ignore_working_copy: bool,
    ) -> Result<Self, CommandError> {
        let transfer = format!("{:032x}", rand::random::<u128>());
        let mut helper = load_helper(ui, command, root, ignore_working_copy).await?;
        jj_lib::git::get_git_backend(helper.repo().store())?;
        if !matches!(
            helper.repo().op_store().name(),
            SimpleOpStore::NAME | SimpleOpStore::SYNC_NAME
        ) {
            return Err(user_error(
                "Repository synchronization requires the built-in operation store",
            ));
        }
        let root = helper.workspace_root().to_owned();
        if helper.repo().op_store().name() == SimpleOpStore::NAME {
            writeln!(
                ui.status(),
                "Enabling repository synchronization. Other processes must be stopped for this \
                 first format transition; already-open old jj processes are not excluded by its \
                 marker."
            )?;
            Endpoint::enable_quiescent(helper.repo_path()).map_err(failed)?;
            helper = load_helper(ui, command, &root, ignore_working_copy).await?;
        }
        let endpoint = Endpoint::load(helper.repo_path());
        if endpoint
            .state_store()
            .read()
            .map_err(failed)?
            .local_anchor
            .is_none()
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
        let inventory = objects(&helper, &endpoint)?
            .inventory(&capture.raw_heads, &capture.shared_heads)
            .map_err(failed)?;
        Ok(Self {
            helper,
            endpoint,
            capture,
            inventory,
            root,
            ignore_working_copy,
            pins: vec![transfer.clone()],
            publication_leases: Vec::new(),
            transfer,
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
        self.advertise_inventory(&self.inventory, &self.capture.current_shared)
    }

    fn advertise_inventory(
        &self,
        inventory: &Inventory,
        shared: &SharedEventId,
    ) -> Result<Advertisement, CommandError> {
        Ok(Advertisement {
            peer: self.endpoint.identity().map_err(failed)?,
            raw_heads: inventory.raw_heads.iter().map(|id| id.hex()).collect(),
            shared_heads: inventory.shared_heads.iter().map(|id| id.hex()).collect(),
            current_shared: shared.hex(),
        })
    }

    fn send_offer(
        &self,
        stream: &mut impl Stream,
        inventory: &Inventory,
        shared: &SharedEventId,
        known: &BTreeSet<RecordId>,
    ) -> Result<(), CommandError> {
        send_command(
            stream,
            "offer",
            &self.advertise_inventory(inventory, shared)?,
        )?;
        let objects = objects(&self.helper, &self.endpoint)?;
        for id in inventory.missing(known) {
            objects
                .export_records(id, |record| {
                    let bytes = record.encode()?;
                    stream
                        .send(&Message::Record(bytes))
                        .map_err(|err| SyncError::InvalidData(command_error_message(&err)))
                })
                .map_err(failed)?;
        }
        stream.send(&Message::Done(Vec::new()))
    }

    fn receive_offer(
        &self,
        stream: &mut impl Stream,
        expected_peer: &str,
        captured: Option<&Advertisement>,
    ) -> Result<Advertisement, CommandError> {
        let offered: Advertisement = receive_command(stream, "offer")?;
        offered.validate()?;
        if offered.peer != expected_peer {
            return Err(user_error("Peer identity changed during synchronization"));
        }
        if let Some(captured) = captured {
            check_captured_offer(&offered, captured)?;
        }
        let objects = objects(&self.helper, &self.endpoint)?;
        let mut quarantine = Quarantine::with_limits(
            &self.helper.repo_path().join("sync"),
            self.helper.repo().store().root_commit_id().clone(),
            self.quarantine_limits()?,
        )
        .map_err(failed)?;
        loop {
            match stream.receive()? {
                Message::Record(bytes) => {
                    quarantine
                        .stage(&objects, &Record::decode(&bytes).map_err(failed)?)
                        .map_err(failed)?;
                }
                Message::Done(bytes) if bytes.is_empty() => break,
                _ => {
                    return Err(user_error(
                        "Unexpected message during synchronization object transfer",
                    ));
                }
            }
        }
        let inventory = quarantine
            .validate(&objects, &offered.raw_heads()?, &offered.shared_heads()?)
            .map_err(failed)?;
        self.endpoint
            .install_and_retain(self.helper.repo(), &quarantine, &inventory, &offered.peer)
            .map_err(failed)?;
        Ok(offered)
    }

    fn known_cache_limit(&self) -> Result<usize, CommandError> {
        Ok(
            positive_count_setting(self.helper.settings(), "sync.max-records")?
                .unwrap_or(MAX_KNOWN_RECORD_IDS)
                .min(MAX_KNOWN_RECORD_IDS),
        )
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

    async fn select(&self, incoming: &Advertisement) -> Result<SharedEventId, CommandError> {
        self.endpoint
            .reconcile(
                self.helper.repo(),
                &self.capture.current_shared,
                &incoming.shared_heads()?,
            )
            .await
            .map_err(failed)
    }

    fn selected_inventory(&self, selected: &SharedEventId) -> Result<Inventory, CommandError> {
        objects(&self.helper, &self.endpoint)?
            .inventory(&self.capture.raw_heads, std::slice::from_ref(selected))
            .map_err(failed)
    }

    async fn apply(
        &mut self,
        ui: &Ui,
        command: &CommandHelper,
        selected: &SharedEventId,
        incoming: &Advertisement,
    ) -> Result<Applied, CommandError> {
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
        let applied = self
            .endpoint
            .reconcile(
                self.helper.repo(),
                &current.current_shared,
                std::slice::from_ref(selected),
            )
            .await
            .map_err(failed)?;
        if let Some(tx) = prepare_integration(
            self.helper.repo(),
            self.endpoint.anchors(),
            &current.current_shared,
            &applied,
        )
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
                incoming_raw_heads: incoming.raw_heads.iter().cloned().collect(),
                incoming_shared_heads: incoming.shared_heads.iter().cloned().collect(),
                applied_anchor: Some(applied.hex()),
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
            captured_anchor: selected.hex(),
            current_anchor: final_capture.current_shared.hex(),
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

async fn capture_current(
    ui: &Ui,
    command: &CommandHelper,
    helper: &mut WorkspaceCommandHelper,
    endpoint: &Endpoint,
    root: &Path,
    ignore: bool,
    transfer: &str,
) -> Result<Capture, CommandError> {
    for _ in 0..8 {
        match endpoint.capture(helper.repo(), transfer).await {
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
    Ok(RepositoryObjects::new(
        operations,
        endpoint.anchors(),
        backend,
    ))
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
    let local_missing = workspace_is_missing(&local_root)?;
    if local_missing && direction == Direction::Send {
        return Err(user_error(
            "Synchronization source repository does not exist",
        ));
    }
    let profile = (!local_missing)
        .then(|| command.load_workspace_for_sync_profile(ui, &local_root))
        .transpose()?;
    if let sync_transport::Endpoint::Local(path) = peer {
        if !workspace_is_missing(path)? {
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
    }
    let mut hello = match &profile {
        Some(profile) => profile_hello(profile, direction, peer.repository_path(), ignore)?,
        None => {
            let mut hello = format_hello(
                direction,
                peer.repository_path(),
                ignore,
                NEGOTIATE_OBJECT_HASH,
            );
            hello
                .required_capabilities
                .push(INITIALIZE_RECEIVER_CAPABILITY.to_owned());
            hello
        }
    };
    drop(profile);
    let mut transport = Transport::connect(peer).map_err(failed)?;
    transport
        .send(&Message::Hello(hello.clone()))
        .map_err(failed)?;
    match transport.receive().map_err(failed)? {
        Message::HelloReply(reply) => {
            if local_missing {
                hello.object_hash.clone_from(&reply.object_hash);
            }
            hello.validate_peer(&reply).map_err(failed)?;
        }
        Message::Error(error) => {
            return Err(user_error(format!("Peer synchronization failed: {error}")));
        }
        _ => {
            return Err(user_error(
                "Peer did not negotiate a synchronization session",
            ));
        }
    }
    if local_missing {
        let object_hash = parse_object_hash(&hello.object_hash)?;
        crate::commands::init_colocated_for_sync(ui, command, &local_root, object_hash).await?;
    }
    let mut local = PreparedEndpoint::open(ui, command, &local_root, ignore).await?;
    hello
        .validate_peer(&local.hello(direction, peer.repository_path())?)
        .map_err(failed)?;
    let local_advertisement = local.advertisement()?;
    send_command(&mut transport, "capture", &local_advertisement)?;
    send_inventory(&mut transport, &local.inventory)?;
    let remote: Advertisement = receive_command(&mut transport, "capture")?;
    remote.validate()?;
    let remote_known = receive_inventory(&mut transport, local.known_cache_limit()?)?;
    #[cfg(feature = "test-fakes")]
    crate::cli_util::pause_for_test("JJ_TEST_SYNC_AFTER_CAPTURE_PAUSE_DIR");
    let local_applied = match direction {
        Direction::Receive | Direction::Bidirectional => {
            let incoming = local.receive_offer(&mut transport, &remote.peer, Some(&remote))?;
            let selected = local.select(&incoming).await?;
            if direction == Direction::Bidirectional {
                let inventory = local.selected_inventory(&selected)?;
                local.send_offer(&mut transport, &inventory, &selected, &remote_known)?;
                let ready: String = receive_command(&mut transport, "ready")?;
                if ready != selected.hex() {
                    return Err(user_error("Peer changed the selected shared result"));
                }
            }
            let applied = local.apply(ui, command, &selected, &incoming).await?;
            if direction == Direction::Bidirectional {
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
                &mut transport,
                &local.inventory,
                &local.capture.current_shared,
                &remote_known,
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
    let peer_diagnostics = transport.finish().map_err(failed)?;
    if !peer_diagnostics.is_empty() {
        writeln!(ui.stderr(), "{peer_diagnostics}")?;
    }
    writeln!(ui.status(), "Synchronization complete.")?;
    Ok(())
}

/// Runs the hidden helper. Stdout belongs exclusively to framed protocol data.
pub(crate) async fn serve(ui: &Ui, command: &CommandHelper) -> Result<(), CommandError> {
    check_command_options(command)?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut session = Session::new(io::BufReader::new(stdin.lock()), stdout.lock());
    let result = serve_session(ui, command, &mut session).await;
    if let Err(error) = &result {
        let _send_error_result =
            Session::send(&mut session, &Message::Error(command_error_message(error)));
    }
    result
}

async fn serve_session(
    ui: &Ui,
    command: &CommandHelper,
    stream: &mut impl Stream,
) -> Result<(), CommandError> {
    let Message::Hello(request) = stream.receive()? else {
        return Err(user_error("Expected synchronization negotiation"));
    };
    let path = sync_transport::decode_repository_path(&request.repository_path).map_err(failed)?;
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
    let mut local = PreparedEndpoint::open(ui, command, &path, request.ignore_working_copy).await?;
    reply
        .validate_peer(&local.hello(request.direction, Vec::new())?)
        .map_err(failed)?;
    let peer: Advertisement = receive_command(stream, "capture")?;
    peer.validate()?;
    let peer_known = receive_inventory(stream, local.known_cache_limit()?)?;
    let advertisement = local.advertisement()?;
    send_command(stream, "capture", &advertisement)?;
    send_inventory(stream, &local.inventory)?;
    match request.direction {
        Direction::Receive => {
            local.send_offer(
                stream,
                &local.inventory,
                &local.capture.current_shared,
                &peer_known,
            )?;
        }
        Direction::Send => {
            let incoming = local.receive_offer(stream, &peer.peer, Some(&peer))?;
            let selected = local.select(&incoming).await?;
            let applied = local.apply(ui, command, &selected, &incoming).await?;
            send_command(stream, "result", &applied)?;
        }
        Direction::Bidirectional => {
            local.send_offer(
                stream,
                &local.inventory,
                &local.capture.current_shared,
                &peer_known,
            )?;
            let incoming = local.receive_offer(stream, &peer.peer, None)?;
            let selected = parse_shared(&incoming.current_shared)?;
            for original in [
                &local.capture.current_shared,
                &parse_shared(&peer.current_shared)?,
            ] {
                if !local
                    .endpoint
                    .anchors()
                    .is_ancestor(original, &selected)
                    .map_err(failed)?
                {
                    return Err(user_error(
                        "Selected shared result does not include both captured histories",
                    ));
                }
            }
            send_command(stream, "ready", &selected.hex())?;
            let requested: String = receive_command(stream, "apply")?;
            if requested != selected.hex() {
                return Err(user_error("Initiator changed the selected shared result"));
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
    jj_lib::git::get_git_backend(loader.store())?;
    if !matches!(
        loader.op_store().name(),
        SimpleOpStore::NAME | SimpleOpStore::SYNC_NAME
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
    Ok(format_hello(direction, path, ignore, hash))
}

fn format_hello(direction: Direction, path: Vec<u8>, ignore: bool, hash: &str) -> Hello {
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
            .map(str::to_owned)
            .collect(),
        required_capabilities: CAPABILITIES.iter().map(|name| (*name).to_owned()).collect(),
    }
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

fn send_inventory(stream: &mut impl Stream, inventory: &Inventory) -> Result<(), CommandError> {
    let mut batch = Vec::with_capacity(INVENTORY_BATCH_SIZE);
    for record in &inventory.records {
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
    if result.current_anchor != result.captured_anchor {
        writeln!(
            ui.status(),
            "{endpoint} applied the captured history; newer local changes remain for the next \
             synchronization."
        )?;
    }
    Ok(())
}

fn check_applied(result: &Applied, selected: Option<&SharedEventId>) -> Result<(), CommandError> {
    parse_raw(&result.operation)?;
    parse_shared(&result.captured_anchor)?;
    parse_shared(&result.current_anchor)?;
    if selected.is_some_and(|id| result.captured_anchor != id.hex()) {
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
        || offered.shared_heads != captured.shared_heads
        || offered.current_shared != captured.current_shared
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
    fn new_receiver_rejects_incompatible_negotiation_before_initialization() {
        let valid = format_hello(Direction::Send, b"/missing".to_vec(), false, "sha1");
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
        send_inventory(&mut output, &inventory).unwrap();
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
            shared_heads: vec!["02".repeat(64)],
            current_shared: "02".repeat(64),
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
        for phase in ["ready", "apply", "result", "complete"] {
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
            shared_heads: vec!["02".repeat(64)],
            current_shared: "02".repeat(64),
        };
        check_captured_offer(&captured, &captured).unwrap();
        for field in 0..4 {
            let mut changed = captured.clone();
            match field {
                0 => changed.peer = "other".to_owned(),
                1 => changed.raw_heads = vec!["03".repeat(64)],
                2 => changed.shared_heads = vec!["04".repeat(64)],
                3 => changed.current_shared = "05".repeat(64),
                _ => unreachable!(),
            }
            assert!(check_captured_offer(&changed, &captured).is_err());
        }
    }
}
