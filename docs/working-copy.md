# Working copy

## Introduction

The working copy is where the current working-copy commit's files are written so
you can interact with them. It is also where files are read from in order to
create new commits (though there are many other ways of creating new commits).

Unlike most other VCSs, Jujutsu will automatically create commits from the
working-copy contents when they have changed. Most `jj` commands you run will
commit the working-copy changes if they have changed. The resulting revision
will replace the previous working-copy revision.

Also unlike most other VCSs, added files are implicitly tracked by default. That
means that if you add a new file to the working copy, it will be automatically
committed once you run e.g. `jj st`. Similarly, if you remove a file from the
working copy, it will implicitly be untracked.

The `snapshot.auto-track` config option controls which paths get automatically
tracked when they're added to the working copy. See the
[fileset documentation](filesets.md) for the syntax. Files with paths matching
[ignore files](#ignored-files) are never tracked automatically.

If you set `snapshot.auto-track` to a non-default value, untracked files can be
tracked with `jj file track`.

You can use `jj file untrack` to untrack a file while keeping it in the working
copy. However, first [ignore](#ignored-files) them or remove them from the
`snapshot.auto-track` patterns; otherwise they will be immediately tracked again.

## Conflicts

When you check out a commit with conflicts, those conflicts need to be
represented in the working copy somehow. However, the file system doesn't
understand conflicts. Jujutsu's solution is to add conflict markers to
conflicted files when it writes them to the working copy. It also keeps track of
the (typically 3) different parts involved in the conflict. Whenever it scans
the working copy thereafter, it parses the conflict markers and recreates the
conflict state from them. You can resolve conflicts by replacing the conflict
markers by the resolved text. You don't need to resolve all conflicts at once.
You can even resolve part of a conflict by updating the different parts of the
conflict marker.

To resolve conflicts in a commit, use `jj new <commit>` to create a working-copy
commit on top. You would then have the same conflicts in the working-copy
commit. Once you have resolved the conflicts, you can inspect the conflict
resolutions with `jj diff`. Then run `jj squash` to move the conflict
resolutions into the conflicted commit. Alternatively, you can edit the commit
with conflicts directly in the working copy by using `jj edit <commit>`. The
main disadvantage of that is that it's harder to inspect the conflict
resolutions.

With the `jj resolve` command, you can use an external merge tool to resolve
conflicts that have 2 sides and a base. There is not yet a good way of
resolving conflicts between directories, files, and symlinks
(<https://github.com/jj-vcs/jj/issues/19>). You can use `jj restore` to choose
one side of the conflict, but there's no way to even see where the involved
parts came from.

## Ignored files

You probably don't want build outputs and temporary files to be under version
control. You can tell Jujutsu to not automatically track certain files by using
`.gitignore` files (there's no such thing as `.jjignore` yet). See
<https://git-scm.com/docs/gitignore> for details about the format. `.gitignore`
files are supported in any directory in the working copy, as well as in
`$XDG_CONFIG_HOME/git/ignore` and `$GIT_DIR/info/exclude`.

Ignored files are never tracked automatically (regardless of the value of
`snapshot.auto-track`), but files that were already tracked will remain tracked
even if they match ignore patterns. You can untrack such files with the
`jj file untrack` command.

## Workspaces

You can have multiple working copies backed by a single repo. Use
`jj workspace add` to create a new working copy. The working copy will have a
`.jj/` directory linked to the main repo. The working copy and the `.jj/`
directory together is called a "workspace". Each workspace can have a different
commit checked out.

When using a Git-colocated repo, `jj workspace add` creates a linked Git
worktree alongside the new workspace so Git commands work inside it.

If you already have a linked Git worktree for the same Git repository, run
`jj workspace adopt --name <name>` from inside it to attach it to the existing
Git-colocated jj repository. The command creates `.jj` metadata in place,
registers a distinct working-copy commit at the worktree's existing Git HEAD,
and then snapshots any local changes without rewriting the worktree files,
Git HEAD, or index.

Run `jj util subvolume init <new-path>` in the main Git-colocated workspace
to build a snapshot-backed replacement checkout. The command leaves the source
checkout untouched, creates a Btrfs subvolume at the new path, copies the
repository into it, creates a nested subvolume for `.git`, and establishes
the initial AWACS baseline there. After it succeeds, rename the old and new
directories to put the initialized checkout at the desired path. If the source
root or `.git` is already a subvolume, initialization snapshots that boundary
instead of copying it.

`jj util subvolume enable` uses that same initializer with a unique sibling
path. It does not rename the source while copying, building, or establishing
the initial baseline. Only after initialization succeeds does it rename the
original checkout aside and activate the initialized checkout at the original
path. It keeps the displaced original as a visible sibling and prints both the
command to enter the activated checkout and a command to delete the original
after it has been verified. By default it removes partial staged checkouts
after a failed initialization; pass `--keep` to retain a failed staging
checkout for inspection. If initialization fails or is interrupted, the
original path is untouched.
Passing `--compress=true` or `--compress=false` sets the corresponding
Btrfs compression property on the newly created root and `.git` subvolumes
and copies file data instead of reflinking it, so existing extents are
rewritten under the requested policy.

While that mode is enabled, `jj workspace add` creates a Btrfs snapshot of the
current checkout automatically. This preserves already-materialized files such as
ignored build outputs while assigning the new workspace its own `.jj` metadata.
In a Git-colocated repo, the command also creates a new linked Git worktree
identity without rewriting the copied files. Run `jj util subvolume disable`
to convert the repository and `.git` back to ordinary directories.

`jj workspace adopt` can also attach an existing linked Git worktree while
subvolume mode is enabled, but only if that worktree root is already a Btrfs
subvolume. Adoption records the inherited files as the initial snapshot
baseline without rewriting them; it rejects a plain directory rather than
silently creating a non-snapshot-backed workspace.

The linked Git worktree may check out a different ref after the child
subvolume is created. Adoption still binds the copied JJ tree to the initial
immutable snapshot first, then its normal snapshot reconciles that baseline
to the Git checkout through the authenticated filesystem delta.

Having multiple workspaces can be useful for running long-running tests in one
while you continue developing in another, for example. If needed,
`jj workspace root --name <workspace>` prints the root path of the specified
workspace (defaults to the current one).

`jj workspace list` shows every workspace together with its available root path.

Use `jj workspace remove <name>` to forget a workspace and remove its files
from disk. Subvolume targets use `btrfs subvolume delete`; ordinary directories
use regular directory removal. Unprivileged subvolume deletion requires the
Btrfs filesystem to be mounted with `user_subvol_rm_allowed`. If deletion
fails, the workspace is still forgotten and `jj` prints a
`sudo btrfs subvolume delete` command for manually removing the remaining
subvolume.

Use `jj workspace forget` when you only want to make the repo forget about a
workspace. The files can be deleted from disk separately (either before or
after).

## Stale working copy

Almost all commands go through three main steps:

1. Snapshot the working copy (which gets recorded as an operation)
2. Create new commits etc. "in memory" and record that as a new operation
3. Update the working copy to match the new operation, i.e. to the commit that
   the operation says that `@` should point to

If step 3 doesn't happen for some reason, the working copy is considered
"stale". We can detect that because the working copy (`.jj/working_copy/`)
keeps track of which operation it was last updated to. When the working copy is
stale, use `jj workspace update-stale` to update the files in the working copy.

A common reason that step 3 doesn't happen for a working copy is that you
rewrote the commit from another workspace. When you modify workspace A's
working-copy commit from workspace B, workspace A's working copy will become
stale.

A working copy can also become stale because some error, such as `^C` prevented
step 3 from completing. It's also possible that it was successfully updated in
step 3 but the operation has then been lost (e.g. by `jj op abandon` or
"spontaneously" by certain storage backends). If the operation has been lost,
then `jj workspace update-stale` will create a recovery commit with the
contents of the working copy but parented to the current operation's
working-copy commit.
