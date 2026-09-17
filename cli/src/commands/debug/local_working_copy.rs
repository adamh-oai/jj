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

use std::fmt::Debug;
use std::io::Write as _;

use jj_lib::working_copy::WorkingCopy as _;

use super::check_local_disk_wc;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::ui::Ui;

/// Show information about the local working copy state
///
/// This command only works with a standard local-disk working copy.
#[derive(clap::Args, Clone, Debug)]
pub struct DebugLocalWorkingCopyArgs {}

pub async fn cmd_debug_local_working_copy(
    ui: &mut Ui,
    command: &CommandHelper,
    _args: &DebugLocalWorkingCopyArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let wc = check_local_disk_wc(workspace_command.working_copy())?;
    writeln!(ui.stdout(), "Current operation: {:?}", wc.operation_id())?;
    writeln!(ui.stdout(), "Current tree: {:?}", wc.tree()?)?;
    let journal = wc.journal_status()?;
    writeln!(ui.stdout(), "Journal phase: {}", journal.phase)?;
    writeln!(ui.stdout(), "Journal generation: {}", journal.generation)?;
    if let (Some(backend), Some(identity)) = (
        journal.baseline_backend.as_deref(),
        journal.baseline_snapshot_identity.as_deref(),
    ) {
        writeln!(ui.stdout(), "Journal baseline: {backend} {identity:02x?}")?;
        if let Some(retention) = journal.baseline_retention {
            writeln!(ui.stdout(), "Journal retention: {retention}")?;
        }
    } else {
        writeln!(ui.stdout(), "Journal baseline: none")?;
    }
    if let Some(reason) = journal.fallback_reason {
        writeln!(ui.stdout(), "Journal fallback: {reason}")?;
    }
    if let Some(mutation) = journal.pending_mutation {
        writeln!(ui.stdout(), "Journal pending mutation: {mutation}")?;
    }
    Ok(())
}
