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

use std::path::PathBuf;

use crate::command_error::CommandError;
use crate::command_error::user_error_with_message;
use crate::sync_git_transport;

/// Connect a native Git subprocess to its parent's private synchronization pipe
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct SyncGitBridgeArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    token: String,
    service: String,
}

pub(crate) fn cmd_sync_git_bridge(args: &SyncGitBridgeArgs) -> Result<(), CommandError> {
    sync_git_transport::run_bridge(&args.socket, &args.token, &args.service)
        .map_err(|err| user_error_with_message("Native Git synchronization bridge failed", err))
}
