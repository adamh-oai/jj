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

use std::ffi::OsString;
use std::path::PathBuf;

use crate::command_error::CommandError;
use crate::command_error::user_error_with_message;
use crate::sync_rsync_transport;

/// Connect an rsync subprocess to its parent's private synchronization pipe
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct SyncRsyncBridgeArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    token: String,
    // Rsync appends its remote-shell service arguments. The peer instead runs
    // a fixed command with private local paths; none of these are executed.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    remote_command: Vec<OsString>,
}

pub(crate) fn cmd_sync_rsync_bridge(args: &SyncRsyncBridgeArgs) -> Result<(), CommandError> {
    let _remote_command = &args.remote_command;
    sync_rsync_transport::run_bridge(&args.socket, &args.token)
        .map_err(|err| user_error_with_message("Immutable-file synchronization bridge failed", err))
}
