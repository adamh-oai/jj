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

use jj_lib::sync::protocol::Direction;

use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error_with_message;
use crate::sync_endpoint;
use crate::sync_transport::Endpoint;
use crate::ui::Ui;

/// Exchange repository data and operation history with another jj repository
///
/// The peer is an existing workspace path or an SSH URL such as
/// ssh://user@host/path/to/repository. Select exactly one direction. Both
/// endpoints snapshot their selected working copies by default; use
/// --ignore-working-copy to skip snapshots and checkout updates at both ends.
/// Workspace names, checkout pointers, and local Git observations stay local.
///
/// The first use enables a repository format that older jj versions reject.
/// Stop other repository processes before that first transition. An already
/// running old process is not excluded by the new format marker.
#[derive(clap::Args, Clone, Debug)]
#[command(group(clap::ArgGroup::new("direction").required(true).multiple(false).args(["receive", "send", "bidi"])))]
pub(crate) struct SyncArgs {
    /// Receive the peer's captured repository history
    #[arg(long)]
    receive: bool,
    /// Send local captured repository history to the peer
    #[arg(long)]
    send: bool,
    /// Exchange histories and apply one shared captured result at both ends
    #[arg(long)]
    bidi: bool,
    /// Existing peer workspace path or ssh://[user@]host[:port]/path
    #[arg(value_name = "PEER", value_hint = clap::ValueHint::DirPath, allow_hyphen_values = true)]
    peer: OsString,
}

pub(crate) async fn cmd_sync(
    ui: &Ui,
    command: &CommandHelper,
    args: &SyncArgs,
) -> Result<(), CommandError> {
    let direction = if args.receive {
        Direction::Receive
    } else if args.send {
        Direction::Send
    } else {
        debug_assert!(args.bidi);
        Direction::Bidirectional
    };
    let endpoint = Endpoint::parse(&args.peer)
        .map_err(|err| user_error_with_message("Cannot open synchronization endpoint", err))?;
    sync_endpoint::synchronize(ui, command, &endpoint, direction).await
}
