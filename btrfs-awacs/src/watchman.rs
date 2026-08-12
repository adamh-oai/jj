//! Focused Watchman command dispatcher used by jj and the hardened Git adapter.

use crate::bser::{decode_frame, encode_frame, Limits, Value};
use crate::compat::ClientFlavor;
use crate::facade::{CompletedQueryCut, FacadeService, PendingQueryCut, PreparedQueryResult};
use crate::manager::{Permissions, Principal, PERMISSION_CUT, PERMISSION_READ, PERMISSION_TRIGGER};
use crate::namespace::ViewBinding;
use crate::service::InitializeOptions;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

pub const VERSION: &[u8] = b"btrfs-awacs-0.1";

#[derive(Clone, Debug)]
struct Registration {
    watch_id: [u8; 16],
    authorization_id: [u8; 16],
    requester_uid: u32,
    requester_gid: u32,
}

#[derive(Default)]
pub struct WatchmanEndpoint {
    roots: RwLock<BTreeMap<Vec<u8>, Registration>>,
    trigger_runner_enabled: bool,
    precision_marker_directory: Option<PathBuf>,
}

pub struct PreparedWatchmanFrame {
    pub(crate) bytes: Vec<u8>,
    query: Option<PreparedQueryResult>,
}

enum ConcurrentResponseKind {
    Clock,
    Query {
        flavor: ClientFlavor,
        limits: Limits,
    },
}

pub struct PendingConcurrentWatchmanFrame {
    cut: PendingQueryCut,
    kind: ConcurrentResponseKind,
}

pub struct CompletedConcurrentWatchmanFrame {
    cut: CompletedQueryCut,
    kind: ConcurrentResponseKind,
}

impl PendingConcurrentWatchmanFrame {
    pub fn execute(self) -> Result<CompletedConcurrentWatchmanFrame, WatchmanError> {
        Ok(CompletedConcurrentWatchmanFrame {
            cut: self
                .cut
                .execute()
                .map_err(|error| WatchmanError::context("run concurrent query cut", error))?,
            kind: self.kind,
        })
    }
}

struct PreparedWatchmanResponse {
    value: Value,
    query: Option<PreparedQueryResult>,
}

impl PreparedWatchmanResponse {
    fn immediate(value: Value) -> Self {
        Self { value, query: None }
    }
}

impl WatchmanEndpoint {
    /// Encodes a well-formed command error as a Watchman response. Transport
    /// and authentication failures are deliberately not sent through this
    /// path because their framing or peer identity is not trustworthy.
    pub fn prepare_error_frame(
        &self,
        error: WatchmanError,
        limits: Limits,
    ) -> Result<PreparedWatchmanFrame, WatchmanError> {
        encode_semantic_response(Err(error), limits)
            .map(|bytes| PreparedWatchmanFrame { bytes, query: None })
    }

    pub fn enable_fixed_jj_trigger(&mut self) {
        self.trigger_runner_enabled = true;
    }

    pub fn enable_precision_guard(&mut self, marker_directory: PathBuf) {
        self.precision_marker_directory = Some(marker_directory);
    }

    pub fn register(
        &self,
        facade: &mut FacadeService,
        root: &Path,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        requester_uid: u32,
        requester_gid: u32,
    ) -> Result<(), WatchmanError> {
        let root = std::fs::canonicalize(root)
            .map_err(|error| WatchmanError::context("canonicalize watch root", error))?;
        match facade.view_binding(watch_id) {
            Some(binding) if binding.root_path == root.as_os_str().as_bytes() => {}
            Some(_) => {
                return Err(WatchmanError::new(
                    "active watch binding differs from its registration root",
                ));
            }
            None => facade
                .activate(watch_id, authorization_id, &root)
                .map_err(|error| WatchmanError::context("activate watch root", error))?,
        }
        if let Some(marker_directory) = &self.precision_marker_directory {
            if !facade.has_precision_guard(watch_id) {
                if let Err(error) =
                    facade.activate_precision_guard(watch_id, marker_directory, current_time_ns()?)
                {
                    // The recursive journal is only a precision/latency aid.
                    // Its activation path durably gaps any epoch it began; the
                    // mandatory snapshot facade remains conservative and must
                    // not be denied merely because inotify is unavailable.
                    eprintln!(
                        "btrfs-awacs: precision guard unavailable for {}: {error}; using snapshot-only invalidation",
                        root.display()
                    );
                }
            }
        }
        self.roots
            .write()
            .map_err(|_| WatchmanError::new("watch registration lock is poisoned"))?
            .insert(
                root.as_os_str().as_bytes().to_vec(),
                Registration {
                    watch_id,
                    authorization_id,
                    requester_uid,
                    requester_gid,
                },
            );
        Ok(())
    }

    pub fn handle(
        &self,
        facade: &mut FacadeService,
        request: &Value,
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
    ) -> Result<Value, WatchmanError> {
        let prepared = self.prepare(
            facade,
            request,
            requester_uid,
            requester_gid,
            now_ns,
            Limits::default(),
        )?;
        let PreparedWatchmanResponse { value, query } = prepared;
        if let Some(query) = query {
            facade
                .finish_query_response(query)
                .map_err(|error| WatchmanError::context("release Watchman response", error))?;
        }
        Ok(value)
    }

    fn prepare(
        &self,
        facade: &mut FacadeService,
        request: &Value,
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
        limits: Limits,
    ) -> Result<PreparedWatchmanResponse, WatchmanError> {
        let command = array(request, "Watchman command")?;
        let name = bytes(
            command
                .first()
                .ok_or_else(|| WatchmanError::new("empty Watchman command"))?,
            "command name",
        )?;
        match name {
            b"watch-project" => self
                .watch_project(facade, command, requester_uid, requester_gid, now_ns)
                .map(PreparedWatchmanResponse::immediate),
            b"clock" => self.clock(facade, command, requester_uid, requester_gid, now_ns),
            b"query" => self.query(
                facade,
                command,
                requester_uid,
                requester_gid,
                now_ns,
                limits,
            ),
            b"trigger-list" => self
                .trigger_list(facade, command, requester_uid, requester_gid)
                .map(PreparedWatchmanResponse::immediate),
            b"trigger-del" => self
                .trigger_delete(facade, command, requester_uid, requester_gid)
                .map(PreparedWatchmanResponse::immediate),
            b"trigger" => self
                .trigger_register(facade, command, requester_uid, requester_gid)
                .map(PreparedWatchmanResponse::immediate),
            _ => Err(WatchmanError::new(format!(
                "unsupported Watchman command {:?}",
                String::from_utf8_lossy(name)
            ))),
        }
    }

    /// Decode one complete BSER-v2 PDU, dispatch it, and encode exactly one
    /// response PDU. Transport framing errors are returned to the caller so it
    /// can close the connection; semantic command errors are ordinary
    /// Watchman error responses.
    pub fn handle_frame(
        &self,
        facade: &mut FacadeService,
        frame: &[u8],
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
        limits: Limits,
    ) -> Result<Vec<u8>, WatchmanError> {
        let prepared =
            self.prepare_frame(facade, frame, requester_uid, requester_gid, now_ns, limits)?;
        let bytes = prepared.bytes.clone();
        self.finish_frame(facade, prepared)?;
        Ok(bytes)
    }

    pub fn prepare_frame(
        &self,
        facade: &mut FacadeService,
        frame: &[u8],
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
        limits: Limits,
    ) -> Result<PreparedWatchmanFrame, WatchmanError> {
        let request = decode_frame(frame, limits)
            .map_err(|error| WatchmanError::context("decode BSER request", error))?;
        let prepared = match self.prepare(
            facade,
            &request,
            requester_uid,
            requester_gid,
            now_ns,
            limits,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                return encode_semantic_response(Err(error), limits)
                    .map(|bytes| PreparedWatchmanFrame { bytes, query: None });
            }
        };
        let PreparedWatchmanResponse { value, query } = prepared;
        match encode_semantic_response(Ok(value), limits) {
            Ok(bytes) => Ok(PreparedWatchmanFrame { bytes, query }),
            Err(error) => {
                if let Some(query) = query {
                    facade.finish_query_response(query).map_err(|release| {
                        WatchmanError::new(format!(
                            "encode Watchman response: {error}; release response fence: {release}"
                        ))
                    })?;
                }
                Err(error)
            }
        }
    }

    pub fn begin_concurrent_frame(
        &self,
        facade: &mut FacadeService,
        request: &Value,
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
        limits: Limits,
    ) -> Result<Option<PendingConcurrentWatchmanFrame>, WatchmanError> {
        let command = array(request, "Watchman command")?;
        let Some(name) = command.first().and_then(|value| match value {
            Value::Bytes(name) => Some(name.as_slice()),
            _ => None,
        }) else {
            return Ok(None);
        };
        let (registration, old_clock, flavor, kind) = match name {
            b"clock" => {
                if !(2..=3).contains(&command.len()) {
                    return Err(WatchmanError::new(
                        "clock requires a root and optional options",
                    ));
                }
                let root = bytes(&command[1], "clock root")?;
                let registration = self.registration(root, requester_uid, requester_gid)?;
                if let Some(options) = command.get(2) {
                    let options = object_ref(options, "clock options")?;
                    if options.len() != 1
                        || options.get(b"sync_timeout".as_slice()) != Some(&Value::Integer(60_000))
                    {
                        return Err(WatchmanError::new("unsupported clock options"));
                    }
                }
                (
                    registration,
                    None,
                    ClientFlavor::Jj,
                    ConcurrentResponseKind::Clock,
                )
            }
            b"query" => {
                if command.len() != 3 {
                    return Err(WatchmanError::new("query requires root and query object"));
                }
                let root = bytes(&command[1], "query root")?;
                let registration = self.registration(root, requester_uid, requester_gid)?;
                let query = object_ref(&command[2], "query options")?;
                let supported: BTreeSet<&[u8]> = [
                    b"since".as_slice(),
                    b"expression".as_slice(),
                    b"fields".as_slice(),
                    b"sync_timeout".as_slice(),
                ]
                .into_iter()
                .collect();
                if query.keys().any(|key| !supported.contains(key.as_slice())) {
                    return Err(WatchmanError::new("query contains an unsupported field"));
                }
                if query.get(b"fields".as_slice())
                    != Some(&Value::Array(vec![Value::Bytes(b"name".to_vec())]))
                {
                    return Err(WatchmanError::new("query fields must be exactly [name]"));
                }
                if let Some(timeout) = query.get(b"sync_timeout".as_slice()) {
                    if timeout != &Value::Integer(60_000) {
                        return Err(WatchmanError::new("unsupported query sync_timeout"));
                    }
                }
                let flavor = classify_expression(
                    query
                        .get(b"expression".as_slice())
                        .ok_or_else(|| WatchmanError::new("query expression is required"))?,
                )?;
                let old_clock = match query.get(b"since".as_slice()) {
                    None | Some(Value::Integer(_)) => None,
                    Some(Value::Bytes(value)) => Some(
                        std::str::from_utf8(value)
                            .map_err(|_| WatchmanError::new("since clock is not ASCII"))?
                            .to_owned(),
                    ),
                    Some(_) => {
                        return Err(WatchmanError::new("unsupported since clock form"));
                    }
                };
                (
                    registration,
                    old_clock,
                    flavor,
                    ConcurrentResponseKind::Query { flavor, limits },
                )
            }
            _ => return Ok(None),
        };
        let cut = facade
            .begin_concurrent_query(
                registration.watch_id,
                old_clock.as_deref(),
                flavor,
                registration.requester_uid,
                registration.requester_gid,
                now_ns,
            )
            .map_err(|error| WatchmanError::context("begin concurrent query", error))?;
        Ok(Some(PendingConcurrentWatchmanFrame { cut, kind }))
    }

    pub fn finish_concurrent_frame(
        &self,
        facade: &mut FacadeService,
        completed: CompletedConcurrentWatchmanFrame,
    ) -> Result<PreparedWatchmanFrame, WatchmanError> {
        let mut prepared = facade
            .finish_concurrent_query(completed.cut)
            .map_err(|error| WatchmanError::context("finish concurrent query", error))?;
        let response_limits = match &completed.kind {
            ConcurrentResponseKind::Query { limits, .. } => *limits,
            ConcurrentResponseKind::Clock => Limits::default(),
        };
        let value = match completed.kind {
            ConcurrentResponseKind::Clock => object([
                (b"version", Value::Bytes(VERSION.to_vec())),
                (
                    b"clock",
                    Value::Bytes(prepared.result.clock.as_bytes().to_vec()),
                ),
            ]),
            ConcurrentResponseKind::Query { flavor, limits } => {
                if !projection_fits_frame(
                    &prepared.result.projection.paths,
                    &prepared.result.clock,
                    limits,
                ) {
                    prepared.result.projection.fresh_instance = true;
                    prepared.result.projection.paths = vec![b"/".to_vec()];
                }
                if flavor == ClientFlavor::Jj
                    && prepared
                        .result
                        .projection
                        .paths
                        .iter()
                        .any(|path| std::str::from_utf8(path).is_err())
                {
                    facade.finish_query_response(prepared).map_err(|error| {
                        WatchmanError::context("release rejected jj response", error)
                    })?;
                    return Err(WatchmanError::new(
                        "jj Watchman facade cannot represent a non-UTF-8 path",
                    ));
                }
                object([
                    (b"version", Value::Bytes(VERSION.to_vec())),
                    (
                        b"clock",
                        Value::Bytes(prepared.result.clock.as_bytes().to_vec()),
                    ),
                    (
                        b"is_fresh_instance",
                        Value::Bool(prepared.result.projection.fresh_instance),
                    ),
                    (
                        b"files",
                        Value::Array(
                            prepared
                                .result
                                .projection
                                .paths
                                .iter()
                                .cloned()
                                .map(Value::Bytes)
                                .collect(),
                        ),
                    ),
                ])
            }
        };
        match encode_semantic_response(Ok(value), response_limits) {
            Ok(bytes) => Ok(PreparedWatchmanFrame {
                bytes,
                query: Some(prepared),
            }),
            Err(error) => {
                facade.finish_query_response(prepared).map_err(|release| {
                    WatchmanError::new(format!(
                        "encode Watchman response: {error}; release response fence: {release}"
                    ))
                })?;
                Err(error)
            }
        }
    }

    pub fn finish_frame(
        &self,
        facade: &mut FacadeService,
        prepared: PreparedWatchmanFrame,
    ) -> Result<(), WatchmanError> {
        if let Some(query) = prepared.query {
            facade.finish_query_response(query).map_err(|error| {
                WatchmanError::context("release Watchman response fence", error)
            })?;
        }
        Ok(())
    }

    pub fn authorize_request<'a>(
        &self,
        facade: &'a FacadeService,
        request: &Value,
        requester_uid: u32,
        requester_gid: u32,
    ) -> Result<&'a ViewBinding, WatchmanError> {
        let command = array(request, "Watchman command")?;
        let root = bytes(
            command
                .get(1)
                .ok_or_else(|| WatchmanError::new("Watchman command omitted its root"))?,
            "Watchman root",
        )?;
        let registration = self.registered(root, requester_uid, requester_gid)?;
        let registration = match registration {
            Some(registration) => registration,
            None if command.first().and_then(|value| match value {
                Value::Bytes(name) => Some(name.as_slice()),
                _ => None,
            }) == Some(b"watch-project") =>
            {
                self.roots
                    .read()
                    .map_err(|_| WatchmanError::new("watch registration lock is poisoned"))?
                    .values()
                    .find(|registration| {
                        registration.requester_uid == requester_uid
                            && registration.requester_gid == requester_gid
                    })
                    .cloned()
                    .ok_or_else(|| {
                        WatchmanError::new(
                            "cannot authorize a new root without an existing namespace view",
                        )
                    })?
            }
            None => return Err(WatchmanError::new("directory is not watched")),
        };
        facade
            .view_binding(registration.watch_id)
            .ok_or_else(|| WatchmanError::new("watch facade is not active"))
    }

    fn watch_project(
        &self,
        facade: &mut FacadeService,
        command: &[Value],
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
    ) -> Result<Value, WatchmanError> {
        if command.len() != 2 {
            return Err(WatchmanError::new("watch-project requires one root"));
        }
        let requested_root = bytes(&command[1], "watch-project root")?;
        let root = std::fs::canonicalize(Path::new(&OsString::from_vec(requested_root.to_vec())))
            .map_err(|error| {
            WatchmanError::context("canonicalize watch-project root", error)
        })?;
        let root_bytes = root.as_os_str().as_bytes().to_vec();
        if self
            .registered(&root_bytes, requester_uid, requester_gid)?
            .is_none()
        {
            let existing = facade
                .service()
                .store()
                .active_uid_watch_at_path(
                    &root_bytes,
                    requester_uid,
                    PERMISSION_READ | PERMISSION_CUT,
                )
                .map_err(|error| WatchmanError::context("find existing watch-project", error))?;
            let (watch_id, grant_id) = match existing {
                Some(existing) => existing,
                None => {
                    let initialized = facade
                        .service_mut()
                        .initialize(
                            &root,
                            &InitializeOptions {
                                principal: Principal::Uid(u64::from(requester_uid)),
                                permissions: Permissions::new(
                                    PERMISSION_READ | PERMISSION_CUT | PERMISSION_TRIGGER,
                                )
                                .map_err(|error| {
                                    WatchmanError::context("build watch grant", error)
                                })?,
                                requester_uid,
                                requester_gid,
                                now_ns,
                            },
                        )
                        .map_err(|error| {
                            WatchmanError::context("initialize watch-project root", error)
                        })?;
                    (initialized.watch_id, initialized.grant_id)
                }
            };
            self.register(
                facade,
                &root,
                watch_id,
                grant_id,
                requester_uid,
                requester_gid,
            )?;
        }
        Ok(object([
            (b"version", Value::Bytes(VERSION.to_vec())),
            (b"watch", Value::Bytes(root_bytes)),
            (b"watcher", Value::Bytes(b"btrfs-index".to_vec())),
        ]))
    }

    fn clock(
        &self,
        facade: &mut FacadeService,
        command: &[Value],
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
    ) -> Result<PreparedWatchmanResponse, WatchmanError> {
        if !(2..=3).contains(&command.len()) {
            return Err(WatchmanError::new(
                "clock requires a root and optional options",
            ));
        }
        let root = bytes(&command[1], "clock root")?;
        let registration = self.registration(root, requester_uid, requester_gid)?;
        if let Some(options) = command.get(2) {
            let options = object_ref(options, "clock options")?;
            if options.len() != 1
                || options.get(b"sync_timeout".as_slice()) != Some(&Value::Integer(60_000))
            {
                return Err(WatchmanError::new("unsupported clock options"));
            }
        }
        let prepared = facade
            .prepare_query(
                registration.watch_id,
                None,
                ClientFlavor::Jj,
                registration.requester_uid,
                registration.requester_gid,
                now_ns,
            )
            .map_err(|error| WatchmanError::context("synchronize clock", error))?;
        Ok(PreparedWatchmanResponse {
            value: object([
                (b"version", Value::Bytes(VERSION.to_vec())),
                (
                    b"clock",
                    Value::Bytes(prepared.result.clock.as_bytes().to_vec()),
                ),
            ]),
            query: Some(prepared),
        })
    }

    fn query(
        &self,
        facade: &mut FacadeService,
        command: &[Value],
        requester_uid: u32,
        requester_gid: u32,
        now_ns: i64,
        limits: Limits,
    ) -> Result<PreparedWatchmanResponse, WatchmanError> {
        if command.len() != 3 {
            return Err(WatchmanError::new("query requires root and query object"));
        }
        let root = bytes(&command[1], "query root")?;
        let registration = self.registration(root, requester_uid, requester_gid)?;
        let query = object_ref(&command[2], "query options")?;
        let supported: BTreeSet<&[u8]> = [
            b"since".as_slice(),
            b"expression".as_slice(),
            b"fields".as_slice(),
            b"sync_timeout".as_slice(),
        ]
        .into_iter()
        .collect();
        if query.keys().any(|key| !supported.contains(key.as_slice())) {
            return Err(WatchmanError::new("query contains an unsupported field"));
        }
        if query.get(b"fields".as_slice())
            != Some(&Value::Array(vec![Value::Bytes(b"name".to_vec())]))
        {
            return Err(WatchmanError::new("query fields must be exactly [name]"));
        }
        if let Some(timeout) = query.get(b"sync_timeout".as_slice()) {
            if timeout != &Value::Integer(60_000) {
                return Err(WatchmanError::new("unsupported query sync_timeout"));
            }
        }
        let flavor = classify_expression(
            query
                .get(b"expression".as_slice())
                .ok_or_else(|| WatchmanError::new("query expression is required"))?,
        )?;
        let old_clock = match query.get(b"since".as_slice()) {
            None | Some(Value::Integer(_)) => None,
            Some(Value::Bytes(value)) => Some(
                std::str::from_utf8(value)
                    .map_err(|_| WatchmanError::new("since clock is not ASCII"))?,
            ),
            Some(_) => return Err(WatchmanError::new("unsupported since clock form")),
        };
        let mut prepared = facade
            .prepare_query(
                registration.watch_id,
                old_clock,
                flavor,
                registration.requester_uid,
                registration.requester_gid,
                now_ns,
            )
            .map_err(|error| WatchmanError::context("run synchronized query", error))?;
        if !projection_fits_frame(
            &prepared.result.projection.paths,
            &prepared.result.clock,
            limits,
        ) {
            prepared.result.projection.fresh_instance = true;
            prepared.result.projection.paths = vec![b"/".to_vec()];
        }
        if flavor == ClientFlavor::Jj
            && prepared
                .result
                .projection
                .paths
                .iter()
                .any(|path| std::str::from_utf8(path).is_err())
        {
            facade
                .finish_query_response(prepared)
                .map_err(|error| WatchmanError::context("release rejected jj response", error))?;
            return Err(WatchmanError::new(
                "jj Watchman facade cannot represent a non-UTF-8 path",
            ));
        }
        let value = object([
            (b"version", Value::Bytes(VERSION.to_vec())),
            (
                b"clock",
                Value::Bytes(prepared.result.clock.as_bytes().to_vec()),
            ),
            (
                b"is_fresh_instance",
                Value::Bool(prepared.result.projection.fresh_instance),
            ),
            (
                b"files",
                Value::Array(
                    prepared
                        .result
                        .projection
                        .paths
                        .iter()
                        .cloned()
                        .map(Value::Bytes)
                        .collect(),
                ),
            ),
        ]);
        Ok(PreparedWatchmanResponse {
            value,
            query: Some(prepared),
        })
    }

    fn trigger_list(
        &self,
        facade: &FacadeService,
        command: &[Value],
        requester_uid: u32,
        requester_gid: u32,
    ) -> Result<Value, WatchmanError> {
        if command.len() != 2 {
            return Err(WatchmanError::new("trigger-list requires one root"));
        }
        let root = bytes(&command[1], "trigger-list root")?;
        let registration = self.registration(root, requester_uid, requester_gid)?;
        let present = facade
            .service()
            .store()
            .has_fixed_jj_trigger(registration.watch_id, registration.authorization_id)
            .map_err(|error| WatchmanError::context("list jj trigger", error))?;
        Ok(object([
            (b"version", Value::Bytes(VERSION.to_vec())),
            (
                b"triggers",
                Value::Array(if present {
                    vec![fixed_trigger_description()]
                } else {
                    Vec::new()
                }),
            ),
        ]))
    }

    fn trigger_register(
        &self,
        facade: &mut FacadeService,
        command: &[Value],
        requester_uid: u32,
        requester_gid: u32,
    ) -> Result<Value, WatchmanError> {
        if !self.trigger_runner_enabled {
            return Err(WatchmanError::new(
                "jj background trigger runner is not configured",
            ));
        }
        if command.len() != 3 {
            return Err(WatchmanError::new("trigger requires a root and definition"));
        }
        let root = bytes(&command[1], "trigger root")?;
        let registration = self.registration(root, requester_uid, requester_gid)?;
        if &command[2] != fixed_trigger_definition() {
            return Err(WatchmanError::new(
                "only jj's fixed background snapshot trigger is supported",
            ));
        }
        let replaced = facade
            .service_mut()
            .store_mut()
            .register_fixed_jj_trigger(registration.watch_id, registration.authorization_id)
            .map_err(|error| WatchmanError::context("register jj trigger", error))?;
        Ok(object([
            (b"version", Value::Bytes(VERSION.to_vec())),
            (
                b"disposition",
                Value::Bytes(
                    if replaced {
                        b"replaced".as_slice()
                    } else {
                        b"created".as_slice()
                    }
                    .to_vec(),
                ),
            ),
            (
                b"triggerid",
                Value::Bytes(b"jj-background-monitor".to_vec()),
            ),
        ]))
    }

    fn trigger_delete(
        &self,
        facade: &mut FacadeService,
        command: &[Value],
        requester_uid: u32,
        requester_gid: u32,
    ) -> Result<Value, WatchmanError> {
        if command.len() != 3
            || bytes(command.get(2).unwrap_or(&Value::Null), "trigger name")?
                != b"jj-background-monitor"
        {
            return Err(WatchmanError::new(
                "trigger-del supports only jj-background-monitor",
            ));
        }
        let root = bytes(&command[1], "trigger-del root")?;
        let registration = self.registration(root, requester_uid, requester_gid)?;
        facade
            .service_mut()
            .store_mut()
            .delete_fixed_jj_trigger(registration.watch_id, registration.authorization_id)
            .map_err(|error| WatchmanError::context("delete jj trigger", error))?;
        Ok(object([
            (b"version", Value::Bytes(VERSION.to_vec())),
            (b"deleted", Value::Bool(true)),
            (b"trigger", Value::Bytes(b"jj-background-monitor".to_vec())),
        ]))
    }

    fn registered(
        &self,
        root: &[u8],
        requester_uid: u32,
        requester_gid: u32,
    ) -> Result<Option<Registration>, WatchmanError> {
        let registrations = self
            .roots
            .read()
            .map_err(|_| WatchmanError::new("watch registration lock is poisoned"))?;
        let Some(registration) = registrations.get(root) else {
            return Ok(None);
        };
        if registration.requester_uid != requester_uid
            || registration.requester_gid != requester_gid
        {
            return Err(WatchmanError::new(
                "request sender does not own this Watchman registration",
            ));
        }
        Ok(Some(registration.clone()))
    }

    fn registration(
        &self,
        root: &[u8],
        requester_uid: u32,
        requester_gid: u32,
    ) -> Result<Registration, WatchmanError> {
        self.registered(root, requester_uid, requester_gid)?
            .ok_or_else(|| WatchmanError::new("directory is not watched"))
    }
}

fn projection_fits_frame(paths: &[Vec<u8>], clock: &str, limits: Limits) -> bool {
    if paths.len() > limits.collection_items
        || clock.len() > limits.string_bytes
        || paths.iter().any(|path| path.len() > limits.string_bytes)
    {
        return false;
    }
    // Reserve a conservative fixed allowance for the response object, keys,
    // booleans, collection markers, and integer encodings. Each string needs
    // at most one marker plus a nine-byte length integer.
    paths
        .iter()
        .try_fold(1024_usize.saturating_add(clock.len()), |total, path| {
            total.checked_add(path.len())?.checked_add(10)
        })
        .is_some_and(|bytes| bytes <= limits.frame_bytes)
}

fn current_time_ns() -> Result<i64, WatchmanError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| WatchmanError::context("read precision activation time", error))?;
    i64::try_from(duration.as_nanos())
        .map_err(|_| WatchmanError::new("precision activation time overflow"))
}

fn classify_expression(value: &Value) -> Result<ClientFlavor, WatchmanError> {
    if value == &jj_expression() {
        Ok(ClientFlavor::Jj)
    } else if value == &git_expression() {
        Ok(ClientFlavor::Git)
    } else {
        Err(WatchmanError::new("unsupported query expression"))
    }
}

fn jj_expression() -> Value {
    Value::Array(vec![
        Value::Bytes(b"not".to_vec()),
        Value::Array(vec![
            Value::Bytes(b"anyof".to_vec()),
            Value::Array(vec![
                Value::Bytes(b"name".to_vec()),
                Value::Array(vec![
                    Value::Bytes(b".git".to_vec()),
                    Value::Bytes(b".jj".to_vec()),
                ]),
                Value::Bytes(b"wholename".to_vec()),
            ]),
            Value::Array(vec![
                Value::Bytes(b"dirname".to_vec()),
                Value::Bytes(b".git".to_vec()),
            ]),
            Value::Array(vec![
                Value::Bytes(b"dirname".to_vec()),
                Value::Bytes(b".jj".to_vec()),
            ]),
        ]),
    ])
}

fn git_expression() -> Value {
    Value::Array(vec![
        Value::Bytes(b"not".to_vec()),
        Value::Array(vec![
            Value::Bytes(b"dirname".to_vec()),
            Value::Bytes(b".git".to_vec()),
        ]),
    ])
}

fn fixed_trigger_definition() -> &'static Value {
    use std::sync::OnceLock;
    static DEFINITION: OnceLock<Value> = OnceLock::new();
    DEFINITION.get_or_init(|| {
        object([
            (
                b"command",
                Value::Array(
                    [b"jj".as_slice(), b"--quiet", b"util", b"snapshot"]
                        .into_iter()
                        .map(|part| Value::Bytes(part.to_vec()))
                        .collect(),
                ),
            ),
            (b"expression", jj_expression()),
            (b"name", Value::Bytes(b"jj-background-monitor".to_vec())),
            (b"stderr", Value::Bytes(b">/dev/null".to_vec())),
            (b"stdout", Value::Bytes(b">/dev/null".to_vec())),
        ])
    })
}

fn fixed_trigger_description() -> Value {
    let Value::Object(definition) = fixed_trigger_definition() else {
        unreachable!("fixed trigger definition is an object")
    };
    object([
        (
            b"name",
            definition
                .get(b"name".as_slice())
                .expect("fixed trigger has a name")
                .clone(),
        ),
        (
            b"command",
            definition
                .get(b"command".as_slice())
                .expect("fixed trigger has a command")
                .clone(),
        ),
    ])
}

fn array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value], WatchmanError> {
    match value {
        Value::Array(value) => Ok(value),
        _ => Err(WatchmanError::new(format!("{field} is not an array"))),
    }
}

fn bytes<'a>(value: &'a Value, field: &str) -> Result<&'a [u8], WatchmanError> {
    match value {
        Value::Bytes(value) => Ok(value),
        _ => Err(WatchmanError::new(format!("{field} is not a string"))),
    }
}

fn object_ref<'a>(
    value: &'a Value,
    field: &str,
) -> Result<&'a BTreeMap<Vec<u8>, Value>, WatchmanError> {
    match value {
        Value::Object(value) => Ok(value),
        _ => Err(WatchmanError::new(format!("{field} is not an object"))),
    }
}

fn object<const N: usize>(entries: [(&[u8], Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_vec(), value))
            .collect(),
    )
}

fn encode_semantic_response(
    response: Result<Value, WatchmanError>,
    limits: Limits,
) -> Result<Vec<u8>, WatchmanError> {
    let response = response.unwrap_or_else(|error| {
        object([
            (b"version", Value::Bytes(VERSION.to_vec())),
            (b"error", Value::Bytes(error.to_string().into_bytes())),
        ])
    });
    encode_frame(&response, limits)
        .map_err(|error| WatchmanError::context("encode BSER response", error))
}

#[derive(Debug)]
pub struct WatchmanError {
    message: String,
}

impl WatchmanError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(context: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
    }
}

impl fmt::Display for WatchmanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WatchmanError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_the_two_supported_client_expressions() {
        assert_eq!(
            classify_expression(&jj_expression()).unwrap(),
            ClientFlavor::Jj
        );
        assert_eq!(
            classify_expression(&git_expression()).unwrap(),
            ClientFlavor::Git
        );
        assert!(classify_expression(&Value::Bool(true)).is_err());
    }

    #[test]
    fn oversized_projection_is_detected_before_response_cloning() {
        let limits = Limits {
            frame_bytes: 1200,
            string_bytes: 1024,
            collection_items: 10,
            depth: 8,
        };
        assert!(projection_fits_frame(
            &[b"small".to_vec()],
            "c:clock",
            limits
        ));
        assert!(!projection_fits_frame(
            &[vec![b'x'; 512], vec![b'y'; 512]],
            "c:clock",
            limits
        ));
    }

    #[test]
    fn semantic_errors_are_framed_but_malformed_pdus_are_rejected() {
        let error = encode_semantic_response(
            Err(WatchmanError::new("unsupported command")),
            Limits::default(),
        )
        .unwrap();
        let decoded = decode_frame(&error, Limits::default()).unwrap();
        let Value::Object(decoded) = decoded else {
            panic!("error response is not an object");
        };
        assert!(decoded.contains_key(b"error".as_slice()));
        assert!(decode_frame(b"bad", Limits::default()).is_err());
    }
}
