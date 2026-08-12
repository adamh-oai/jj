# Indexed Btrfs change tracking

Status: normative v1 design plus an experimental implementation. The
`btrfs-awacs` binary retains its `snap` and `compare` benchmark commands and
also implements the manager, privileged broker, persistent index, Worktree,
GC, focused jj-vcs Watchman endpoint, and native Git fsmonitor adapter described
below. Section 12 distinguishes implemented behavior from remaining
stabilization and performance work.

## 1. Goals

The service maintains a persistent namespace index for immutable Btrfs
snapshots. It provides four operations:

1. **Initialize** a writable or read-only subvolume by taking a read-only
   snapshot and building a complete index of that snapshot.
2. **Changes** by taking another read-only snapshot, updating the index from a
   kernel object delta, and returning a durable stream of changed names.
3. **Worktree** by first performing a Changes cut, then taking a writable
   snapshot of that read-only anchor at a caller-selected path.
4. **Garbage collection** of managed snapshots and index history which are no
   longer reachable or pinned.

Snapshot and worktree creation share Btrfs metadata rather than walking file
contents. Index creation is necessarily O(namespace size). An incremental
update should be O(changed B-tree items + changed references + output), subject
to the directory-rename caveat below. "O(1) snapshot" refers to the logical
copy-on-write snapshot operation, not a bound on transaction commit latency or
future copy-on-write allocation.

The design supports branches. A writable worktree shares its seed index
revision; it does not copy all index rows.

The service also exposes the narrow filesystem-monitor compatibility surface
used by jj and Git. jj talks to a focused Watchman-compatible BSER endpoint;
Git uses a native fsmonitor hook-v2 adapter. A hardened JSON adapter over a
focused CLI shim is a later optional compatibility layer, not part of the v1
correctness boundary. The unmodified stock `fsmonitor-watchman` sample is only
a future restricted conformance target because it constructs shell and JSON
strings unsafely. The required adapters project the durable comparison stream
into conservative "paths which may have changed" sets. They are not a general
Watchman replacement.

This design does not transfer file data, support snapshots across filesystems,
or recursively track nested subvolumes in v1. "Arbitrary subvolume" therefore
means any regular subvolume for which the configured manager store is outside
the watched tree. V1 rejects the Btrfs top-level root and any source which is
an ancestor of the manager store; those roots necessarily conflict with the
non-recursive/nested-subvolume policy.

## 2. Btrfs namespace model

### 2.1 Snapshot identity

The durable identity of a managed snapshot is:

```text
(filesystem UUID, subvolume UUID)
```

The database also records the root/tree ID, `ctransid`, `otransid`,
`parent_uuid`, and `received_uuid`. Paths and root IDs are locators, not
identities: paths can be renamed and root IDs can eventually be reused.
`parent_uuid` describes how Btrfs created a subvolume; it is not the logical
predecessor used by the index. In particular, successive snapshots of one
writable subvolume are normally siblings whose `parent_uuid` is that writable
subvolume's UUID.

A managed index revision is valid only while its snapshot remains read-only
and its UUID and recorded transaction metadata still match. The manager owns
its snapshot directory and never changes a managed snapshot back to writable.
Read-only is not physical immutability on Btrfs: `FILE_EXTENT_SAME`/dedupe may
rewrite extent and inode items in an RO subvolume. Managed paths/fds are never
exposed, `send_in_progress` excludes a concurrent dedupe during comparison,
and the manager rechecks UUID plus root transaction metadata before and after
comparison and again at publication. Any change invalidates the revision and
enters recovery; it is never accepted as the intended cut.

### 2.2 Hardlinks

Btrfs does **not** permit a hardlink across subvolumes, including subvolumes on
the same filesystem. `linkat(2)` returns `EXDEV` when the source and destination
root IDs differ. A reflink or snapshot may share extents, but it does not create
a cross-subvolume inode.

Within one subvolume, the index represents:

```text
ino -> {generation, file type, mode, nlink}
ino -> set of (parent_ino, raw_basename_bytes)
```

An ordinary file can therefore have several paths. Directories cannot be
hardlinked, so directory ancestry is unique. Inode identity is scoped to an
index revision; an inode number in another subvolume is not the same object.
`(ino, generation)` distinguishes inode-number reuse along one comparison
lineage.

The changed-object stream behaves as follows:

- A content or metadata change produces one object record. It may have no
  reference records. Userspace looks up all current references in the index
  and reports every hardlink alias.
- Adding or removing one hardlink produces a reference add or delete for the
  same inode. Other aliases remain inherited from the base revision.
- Renaming a link produces a delete of the old `(parent, name)` and an add of
  the new one.
- A cross-subvolume copy or reflink is a new inode in the destination. If the
  source is removed, independently tracking the source reports a deletion;
  there is no shared inode identity tying the two events together.
- Taking a snapshot initially preserves the source's internal hardlink graph,
  which is why a writable worktree can share its seed index revision.

The current changed-object implementation emits every old and new entry when
a packed inode-ref item changes. Consumers must set-difference the triples and
cancel identical add/delete pairs before applying them.

### 2.3 Nested subvolumes

Btrfs snapshots are not recursive. A nested subvolume is a separate root, not
a directory hardlink. The local v2 ABI includes every `DIR_INDEX` item whose
location key is `ROOT_ITEM` and emits raw-name `BOUNDARY_ADD` / `BOUNDARY_DELETE`
records plus the mandatory `BOUNDARY_RECORDS` stream capability. Full-index
mode reports every boundary; delta mode reports changed packed items and
userspace cancels identical add/delete records. Because every accepted base is
proved boundary-free, an effective add proves the target is invalid while an
empty add set proves the invariant remains true in O(changed items). V1
validates the **immutable result of every cut**, not merely the mutable source
before snapshotting, and rejects a snapshot containing a nested boundary.
Legacy kernels without the capability retain a fail-closed namespace scan,
which may not be O(changed objects). A future service version may instead represent a boundary
as an opaque object containing the child subvolume UUID and attach a separate
watch; it must not silently treat the child as an ordinary directory.

V1 also rejects any fscrypt-encrypted directory/inode in the immutable cut.
The Btrfs index contains on-disk encrypted names while jj/Git see names through
the caller's key domain; adding or removing a key can change namespace
visibility without a Btrfs object delta. Supporting fscrypt would require a
VFS-translated index under a pinned key identity plus key-lifecycle events in
the clock continuity model. Likewise, filesystem-monitor activation rejects
existing descendant mountpoints and then continuously monitors the caller's
mount namespace as described in Section 5; a mount-over must never be mistaken
for content of the indexed subvolume.

## 3. Change-stream contract

The public stream is revision based, not wall-clock based. A cursor is
`(watch_id, cut_sequence)`. A committed comparison can always be replayed with
the same event order.

Each comparison is bound to exact `from` and `to` snapshot identities and
contains:

```text
ChangeSet {
    watch_id, sequence, from_snapshot, to_snapshot,
    fresh_instance, events[]
}

Event {
    kind, ino, old_generation?, new_generation?, change_mask,
    old_path?, new_path?
}
```

Names and paths are byte strings. They are not assumed to be UTF-8. Event kinds
include `path-added`, `path-removed`, `path-changed`, and `subtree-moved`.
Creates and deletes contain all new or old aliases. An object-only change
contains every alias in the target revision.

Paths are derived recursively from reference edges:

```text
paths(ino) = paths(parent_ino) + "/" + name
             for every reference (ino, parent_ino, name)
```

Path caches are keyed by revision and inode. A process-global `ino -> path`
cache would be incorrect after a directory move.

A directory rename changes one reference edge while every descendant pathname
changes. The compact stream emits one `subtree-moved(old_prefix, new_prefix)`
event. A caller that requires one event per descendant can request expansion,
which walks the parent index and costs O(subtree size). An adapter for an API
that cannot express subtree moves must expand it or return a fresh-instance
notification; it must not omit the descendants while claiming a complete
Watchman-style result.

A direct historical comparison A -> B reports semantic endpoint differences
plus the required persistent object/directory dirty witnesses between those
immutable snapshots. It cannot reconstruct the exact names, count, or order of
transient operations which disappeared before B; a directory witness says only
that its subtree may have been observed differently. The ordered per-watch
cursor stream retains every cut so exact events can be unioned. If an
intermediate cut is irrecoverably lost, the next stream result is marked
`fresh_instance` rather than silently claiming that a collapsed comparison is
the original event history.

The jj/Git compatibility layer has a stronger requirement than this core
stream. Those clients receive a clock and then inspect the mutable worktree. A
path they observe after that clock can disappear or revert before the next RO
cut, so a comparison of semantic endpoint contents alone would be unsafe.

V1 requires a **persistent dirty-witness** invariant from the kernel ABI. RO
snapshot creation is a transaction barrier. Every later client-visible
mutation must either change an emitted item for the affected surviving inode,
or persistently change an emitted inode item for a surviving parent directory
(and therefore the nearest surviving ancestor of a wholly transient subtree).
The current prototype appears to have this property because Btrfs snapshot
creation commits a transaction, inode-item updates store its `transid` and
sequence, and `CHANGED_OBJECTS` deep-compares those items. This observation
must become a documented ABI promise and an xfstest suite before production.

The compatibility projection treats every changed directory not covered by a
complete precision interval as a coarse subtree invalidation; jj requires a
fresh/full crawl and Git can consume a trailing-slash prefix, with the root
mapped to `/`. Thus a transient mutation
cannot become an empty success even when its exact name no longer exists. An
optional durable namespace guard records exact create/delete/rename names and
wakes triggers so most such intervals can remain incremental. It improves
precision and latency but is not the sole correctness mechanism; if its
coverage is absent or gapped, the service falls back to the dirty witness
rather than trusting a partial event journal.

The dirty witness covers mutations *inside* the watched subvolume, not changes
to the VFS view which selects that subvolume. A transient rename/replacement of
the watched root or one of its ancestors, or a transient mount-over, can be
observed by a client and later disappear without changing the indexed root.
Filesystem-monitor clocks therefore also require continuous root-path-binding
and mount-topology monitors. Any relevant path event, mountinfo event,
overflow, fd loss, or monitor restart rotates the affected clock epoch; the
core snapshot/index API remains usable without these facade-only monitors.

## 4. Filesystem layout

There is one manager store per Btrfs filesystem. The store must be outside all
watched subvolume trees, owned by the manager, and mode `0700`. Its snapshots
directory must be on the same Btrfs filesystem as the watched roots. SQLite
could technically live elsewhere, but keeping the state together simplifies
recovery.

```text
<manager-root>/
    state.sqlite3
    state.sqlite3-wal                 # while SQLite has WAL open
    state.sqlite3-shm
    snapshots/
        <watch-id>/
            s-<sequence>-<operation-id>   # managed read-only subvolumes
    spool/
        <job-id>-<all-fences>.objects.part
        <job-id>-<all-fences>.objects      # winning complete manifest
        <job-id>-<all-fences>.stage.sqlite3  # private index/event staging
    staging/
        worktree-<operation-id>             # protected RW subvolume before publish
    quarantine/                         # unexpected managed-looking objects

/var/lib/btrfs-awacs/broker/            # root-owned, not writable by manager
    receipts.sqlite3                    # privileged-execution journal
```

The privileged broker and each user's compatibility daemon use different
sockets and different trust domains:

```text
/run/btrfs-awacs/
    service.sock                        # manager-owned, grant-checked API
    broker.sock                         # root-owned, manager access only

$XDG_RUNTIME_DIR/btrfs-awacs/           # mode 0700, owned by one user
    mnt-<namespace-dev>-<namespace-ino>/
        watchman.sock                   # mode 0600, focused BSER-v2 endpoint
        daemon.lock
```

The per-user daemon runs in one mount namespace, interprets working-tree
paths, serves Git/jj, and executes an optional jj trigger as that user. It sends
fd-anchored, grant-checked requests to the unprivileged index manager, which
owns SQLite, snapshots, spool files, clocks, and scheduling. Only that manager
can request fixed Btrfs operations from the root broker. The broker neither
speaks Watchman nor executes client commands. A system-wide compatibility
socket would need fd-passing root registration and a real user/process sandbox;
v1 does not provide one.

A UID may have processes in several mount namespaces. Discovery keys the socket
by the caller's mount-namespace identity, and the daemon independently compares
its namespace with the connecting peer's namespace using a pidfd plus
`/proc/<pid>/ns/mnt`. A shared mount namespace is still insufficient because a
process may have a different `fs_struct` root after `chroot`: registration
opens `/proc/<pid>/root`, records its device/inode/mount ID, and resolves the
client's absolute root beneath that fd. The daemon enables `SO_PASSCRED` and
`SO_PASSPIDFD`, and every `recvmsg` span contributing bytes to one request
frame must carry matching kernel-supplied `SCM_CREDENTIALS` and `SCM_PIDFD`.
Mixed/missing credentials reject the frame. The already-bound pidfd—not a
racy later `pidfd_open()` of a potentially recycled numeric PID—anchors
`/proc/<pid>/root` and `ns/mnt` checks before the process exits or changes view.
The daemon then rechecks the sender's current UID, mount namespace, and
process-root identity. This catches post-connect `setns`/`chroot` changes and makes socket
passing attributable to the actual sender rather than treating connection
setup as permanent authority. It does **not** preserve a same-UID process's
narrower Landlock, seccomp, chroot, or LSM policy: v1 deliberately treats all
same-UID processes in the recorded namespace/view as one trusted principal,
and the socket fd is delegable within that principal. A deployment which must
isolate same-UID sandboxes needs separate service principals or a kernel/MAC
capability bound to the narrower security domain; mode `0600`, `SO_PEERCRED`,
and `SCM_CREDENTIALS` are not such a boundary. An explicit `WATCHMAN_SOCK`
does not bypass the UID/view checks.

The database UUID is authoritative; sequence numbers in names are for
operators. Before creation, an intent records the source UUID, FSID, requested
flags, fence, and an unguessable deterministic path under the protected manager
directory. The target UUID is generated by the kernel and cannot be known yet.
After creation, the manager inspects and durably records it. Recovery may adopt
an object without a previously recorded target UUID only at that protected
intent path and only when its FSID, source/parent UUID, flags, and immutable
operation intent match; otherwise it quarantines the object. Btrfs does not
store a SQLite lease fence in the subvolume, so the creating fence cannot be
recovered from disk. Adoption is fence-independent, while only the current DB
fence may publish it. Once recorded, the target UUID must also match.

Writable worktrees are user-visible subvolumes at caller-selected paths. The
destination parent must be on the same Btrfs filesystem as the read-only seed;
otherwise Worktree fails with `EXDEV`. Managed snapshot GC never deletes a
writable worktree implicitly.

The prototype's current `<source>/.btrfs-awacs` layout must not be used for the
service. It mutates the watched namespace, creates nested-subvolume stubs in
later snapshots, and makes lexical timestamp order stand in for durable state.

## 5. Kernel interfaces

### 5.1 Existing interfaces

| Purpose | Interface | Requirements and use |
| --- | --- | --- |
| Identify filesystem | `BTRFS_IOC_FS_INFO` | Read FSID after opening the root. No special capability beyond access to the fd. |
| Identify subvolume | `BTRFS_IOC_GET_SUBVOL_INFO` | Read UUID, parent/received UUID, root ID, and transaction metadata. Recheck after every create and before comparison/publication. |
| Create RO cut or RW clone | `BTRFS_IOC_SNAP_CREATE_V2` | Destination directory fd, source-root fd, and optional `BTRFS_SUBVOL_RDONLY`. Both paths must be on one Btrfs filesystem. Snapshot creation commits a filesystem transaction; the fsmonitor design relies on that ordering barrier. |
| Incremental object delta | Local `BTRFS_IOC_CHANGED_OBJECTS` v2; legacy fallback is experimental `BTRFS_IOC_SEND` with exactly `NO_FILE_DATA` plus `CHANGED_OBJECTS` | V2 receives source and target root fds, requires distinct RO roots on one filesystem, and emits endpoint identities, target attributes/xattrs, nested-boundary transitions, bounded records, and a checksummed completion footer. The legacy parent is a numeric root ID and is accepted only when the dedicated ioctl returns `ENOTTY`. Neither local extension is upstream ABI. |
| Initial exact index | Userspace traversal of the immutable RO snapshot | Enumerate raw directory entries and obtain each reachable object's metadata through ordinary fd-relative VFS operations. The prototype still uses privileged `BTRFS_IOC_TREE_SEARCH_V2` while the VFS walker is implemented; `BTRFS_IOC_CHANGED_OBJECTS` has no full-index mode. |
| Root-path-binding continuity | A separate `inotify_init1` fd watches every parent/component from the pinned process root to the watched subvolume root | Mandatory for clocks unless an equivalent immutable-path policy is enforced. Arm top-down before resolving the next component; relevant create/delete/move/self/ignored events, overflow, unmount, permission loss, or monitor restart rotate the clock epoch. Drain a private marker and re-resolve the complete inode/mount/UUID chain at admission and final response. This fd is separate from the optional recursive precision guard so subtree load cannot silently weaken authority. |
| Optional precise namespace guard | Recursive `inotify_init1` / `inotify_add_watch` in the per-user daemon | Durably records exact create/delete/rename names and wakes triggers between cuts. It requires traversal/watch access to the whole worktree and does not observe every content mechanism (for example writable `mmap`). Content correctness comes from the dirty witness. Queue overflow, watch-install races, permission loss, restart, or an unresolvable event makes the interval imprecise; queries fall back to coarse directory invalidation. |
| Mount-topology continuity | Keep `/proc/self/mountinfo` open in the per-namespace daemon and poll it for `POLLERR`/`POLLPRI` | Mandatory for the client-visible namespace unless deployment can make its topology immutable. The kernel's `mnt_namespace.event` changes on attach/detach even when mountinfo text later returns to the same value. Because poll has no affected-path payload, any event rotates every clock epoch bound to that namespace monitor; reparsing only rejects mounts which remain. Check the poll state when admitting/finalizing every boundary and treat fd/daemon/boot loss as a gap. Local-kernel `FAN_REPORT_MNT`/`FAN_MARK_MNTNS` is an optional richer replacement; recursive inotify alone is insufficient. |
| Delete managed snapshot | `BTRFS_IOC_SNAP_DESTROY_V2` | Run only after the DB marks a snapshot deleting and removes eligibility for new pins. |
| Commit deletion | `BTRFS_IOC_START_SYNC` / `BTRFS_IOC_WAIT_SYNC` (or checked `syncfs`) | Wait for the namespace deletion's transaction to become durable before SQLite says `deleted`. |
| Wait for cleanup | `BTRFS_IOC_SUBVOL_SYNC_WAIT` when needed | Optional for space-reclamation observability only; it is not the namespace-durability barrier. |

The legacy local changed-object stream has a 24-byte magic/version/length
header followed by object records `(ino, old_generation, new_generation,
mask)` and reference records `(ino, parent_ino, raw_name)`. Its masks cover
inode, ref, xattr, data, verity, created, and deleted changes.

Important legacy-v1 limitations are:

- the header has no FSID, source/target UUIDs, or transaction IDs;
- there is no completion record, count, or checksum;
- output bytes may have been written before a later ioctl failure;
- object records do not carry file type, mode, or nlink;
- there is no full-index/bootstrap mode;
- the parent is selected by filesystem-local numeric root ID; and
- output structures are private to `send.c`, not a documented UAPI.

V1 userspace therefore spools all bytes, accepts them only after the ioctl
returns success, fully parses and validates them, hashes the manifest, and
independently binds it to the UUIDs/transaction metadata read around the call.
It never applies a partial pipe directly to SQLite.

The local v2 prototype implements the structural items 1 through 7 below. Its
header binds FSID, both subvolume UUIDs/root IDs/ctransids; object records carry
target inode fields and an explicit change sequence; every surviving emitted
object is followed logically by an exact replacement set for relevant
`security.*`, `trusted.*`, and fscrypt xattrs; and the final record authenticates
the preceding byte and record counts with CRC32C. Unknown record types are
accepted only with the UAPI `OPTIONAL` flag. The manager additionally hashes
the complete private spool before publication. There is no fallback after a
v2 limit, parse, checksum, identity, or execution failure—only an unsupported
ioctl (`ENOTTY`) selects legacy v1. It also sets the output-only
`BOUNDARY_RECORDS` capability and emits mandatory `BOUNDARY_ADD` and
`BOUNDARY_DELETE` records for nested-subvolume `DIR_INDEX` entries. Userspace
refuses v2 streams lacking that capability and refuses a cut with any effective
target boundary addition. The output-only `DIRTY_WITNESS` capability declares
the inode-transid persistence contract. Facade activation requires both the
deployment's explicit conformance opt-in and an observed capability; after a
service restart it independently traverses the retained immutable head and
requires the resulting index to equal SQLite before minting another clock. A
legacy-kernel fallback can still serve the core snapshot/index API but cannot
enable the jj/Git facade.

This is still a prototype rather than the production ABI because items 8
through 10, xfstests, fuzzing, and upstream review remain. It retains the kernel's `CAP_SYS_ADMIN` check and the
broker boundary described in Section 6.

### 5.2 Required production ABI

Before supporting a broadly deployed service, add a documented v2 object
interface (a dedicated ioctl is preferable to another send-stream special
case):

1. Supply **both** source and target as opened root fds. Never accept an
   unverified numeric root ID as an authority boundary.
2. Require both roots to be read-only, alive, on the same filesystem, and root
   inode fds. Hold the existing send-in-progress protection for the duration.
3. Put FSID, source UUID/ctransid, and target UUID/ctransid in the stream
   header, and include a completion footer with record/byte counts. Successful
   ioctl completion plus the footer is the commit indication.
4. Define record structs, byte order, alignment, ordering independence,
   duplicate semantics, and unknown-record extension rules in UAPI docs.
5. Emit only changed inode numbers, generations, and change-class masks. The
   masks include `BTRFS_CHANGED_OBJECT_CHANGE_FILE_DATA` for file payload
   changes and `BTRFS_CHANGED_OBJECT_CHANGE_DIR_ENTRIES` for a directory whose
   child set changed. Do not emit names, references, target
   inode attributes, or xattr names/values. Userspace obtains authorized
   target state through ordinary VFS operations and rescans changed
   directories.
6. Keep bootstrap out of this ABI. Build the initial exact index by traversing
   one immutable RO snapshot in userspace.
7. Bound kernel memory, allow interruption, reschedule long walks, handle output
   backpressure, and permit caller-specified byte/record limits with an
   explicit overflow result.
8. Specify the persistent dirty-witness invariant: snapshot creation orders a
   new epoch, every later client-visible mutation changes a streamed item for
   its surviving inode or a surviving ancestor directory, and that witness
   cannot return equal until after a later cut. Include a monotonic change
   sequence in object records rather than making userspace infer this from a
   private inode-item layout.
9. Add Btrfs tests for hardlinks, packed/extrefs, inode reuse, all object masks,
   non-UTF-8 names, directory moves, partial-output errors, root deletion races,
   unrelated roots, nested-subvolume policy, data/metadata modify-and-restore,
   writable mmap, create/delete of whole transient subtrees, and two cuts
   around every supported mutation mechanism. Attempt `FILE_EXTENT_SAME`
   against a protected RO cut and prove transaction-metadata revalidation
   rejects any mutation.
10. For a future precise kernel-native filesystem-monitor path, expose a per-subvolume
   monotonic mutation journal (object/ref identities, old/new names, overflow
   generation, and a snapshot-correlatable cursor). A second net tree diff is
   not such a journal: it must retain create/delete and modify/revert activity
   between observation cuts. This replaces recursive inotify for precision and
   latency; the dirty-witness fallback remains the correctness backstop.

Property tests compare a fresh userspace traversal of B with
`apply(index(A), delta(A,B))`.

## 6. Privilege and security model

The current changed-object operation is **not safe for arbitrary unprivileged
use**, even if the caller can open or list the snapshot root.

`BTRFS_IOC_SEND` currently has an unconditional `CAP_SYS_ADMIN` check. Merely
removing it for `CHANGED_OBJECTS` would expose raw names, inode relationships,
and change types below directories which the caller cannot traverse. Opening a
subvolume root is not proof of read/search permission on every descendant. The
numeric parent root ID also creates a confused-deputy problem: a caller could
name another otherwise unreachable RO root on the same filesystem. Finally, a
comparison can consume substantial CPU, I/O, and output bandwidth.

After the stream is narrowed to inode numbers and change-class masks, an
administrator may explicitly opt into unprivileged comparisons with a
filesystem-wide mount option analogous to `user_subvol_rm_allowed`. The ioctl
must still require read and search permission on both supplied root fds and
retain resource limits. Such an option explicitly accepts the residual side
channel from stable inode numbers and change activity below inaccessible
descendants; it does not prove every descendant is visible.

V1 uses a small privileged broker, not a `CAP_SYS_ADMIN`-enabled main binary.
The unprivileged manager is the authorization decision point. It authenticates
the per-user daemon as its `service.sock` peer, checks the daemon's active
whole-watch grant and its view-bound request, and mints a fenced,
operation-scoped broker request. The broker sees the manager as its Unix peer;
it does **not** pretend that a UID asserted by the manager is the original app
process. The manager and per-user daemon are therefore in the authorization
TCB; the broker minimizes privileged parsing and constrains consequences, but
is not claimed to contain either a malicious manager which can fabricate its
own SQLite grant rows or a malicious same-UID daemon. A threat model requiring
that containment needs a separate root-owned capability registry or MACed
provisioning handles. The broker:

- accepts requests only over the manager-authenticated channel and only in the
  fixed operation protocol. Operation IDs and fences provide idempotency and
  stale-worker exclusion, not an independent reimplementation of the
  manager-owned authorization database;
- opens managed paths itself (or receives fds), verifies root inode, FSID,
  UUID, read-only state, and transaction metadata;
- independently constrains the operation kind, source/target UUIDs, flags,
  manager-owned paths, output bounds, and fencing token. The manager has
  already checked the explicit whole-watch grant established by an
  administrator or trusted provisioning policy; owning/reading only the root
  directory is not such a grant. `watch_grants` durably binds the watch to
  principals and permissions, and every Changes, historical comparison,
  replay/read, Worktree, caller-directed retention, and watch-deletion request
  rechecks it. Physical snapshot GC is a separately fenced manager-policy
  operation after all pins are gone;
- permits only fixed snapshot, full-index, changed-object, and deletion
  operations on managed roots; Worktree publication is the sole outside-store
  operation and accepts a previously verified destination-parent fd, one
  basename, and the caller-created reservation capability described in Section
  8.3. It verifies their recorded identities, FSID, operation fence, and nesting
  policy and never follows a caller-supplied path string;
- serializes an execution receipt per `(operation ID, target identity)` around
  every privileged filesystem mutation in its root-owned receipt journal.
  Dispatch takes the grant's execution gate, rechecks authorization/fence,
  records the request, and has the broker durably mark it running before the
  ioctl. Revocation takes that gate exclusively: if revocation wins, dispatch
  cannot start; if dispatch wins, revocation records pending and orders after
  exact completion or broker-journal reconciliation. The manager may not
  expire, transfer, abort, or issue a conflicting fence while the receipt is
  running. This is essential for external Worktree rename and snapshot
  deletion: a DB lease fence can prevent stale publication but cannot stop an
  already-started ioctl;
- caps concurrency and output bytes. V2 enforces record/byte limits in-kernel
  and polls pending signals in both incremental comparison and full-index
  traversal, returning `EINTR` with an interrupted status. Wall-clock deadlines
  remain userspace policy: the broker must signal the worker, and a currently
  held kernel lock can still delay reaching the next cancellation point; and
- returns a spooled manifest or a normalized result only after success.

| Operation | Privilege today |
| --- | --- |
| `FS_INFO` / `GET_SUBVOL_INFO` | No capability check in these paths; normal fd/path access still applies. |
| RO tracking snapshot | No unconditional `CAP_SYS_ADMIN`: the caller must own the source subvolume root (or be capable) and be allowed to create in the destination directory. The broker is needed when that is not true. |
| RW worktree snapshot | Same snapshot-creation rules. The caller also needs access to the destination parent. |
| Current changed-object delta | Requires `CAP_SYS_ADMIN`; invoke through the broker. |
| Exact initial `TREE_SEARCH_V2` index | Requires `CAP_SYS_ADMIN`; invoke through the broker. A normal directory crawl is permission-filtered and lacks the exact Btrfs generation data required by the durable model. |
| Managed snapshot deletion | Normally `CAP_SYS_ADMIN`. Unprivileged removal is possible only with the `user_subvol_rm_allowed` mount option plus the relevant directory permissions. Use the broker by default. |
| SQLite reads/writes and path derivation | No kernel capability; Unix permissions on the manager store apply. |
| Mandatory root-path-binding monitor | No capability when the per-user daemon can read/watch every ancestor directory in its own view. V1 disables the fsmonitor facade if any component cannot be watched or inotify coherence is not trusted; the core snapshot API remains available. A privileged notification-only replacement would need a separately constrained broker protocol. |
| Optional recursive inotify precision guard | No capability when the per-user daemon can traverse/watch the whole worktree. If it cannot establish and retain complete coverage, queries remain available through conservative snapshot-only invalidation, but exact transient names and low-latency triggers are unavailable. |
| Mount-topology continuity monitor | Polling an already-open `/proc/self/mountinfo` fd requires no capability and observes the daemon's mount namespace. If the optional `FAN_MARK_MNTNS` interface is used instead, it requires `CAP_SYS_ADMIN` in the fanotify group's user namespace and the broker supplies only a notification-class, mount-event-only fd. |
| Watchman-compatible query / Git hook | Runs in the per-user daemon without capabilities. It checks each frame's actual sender and passes a rooted fd plus view binding to the manager; the manager authenticates that daemon as its service peer and checks the watch grant before asking the broker for a cut. Query expressions are presentation filters, not authorization. |
| jj background trigger | Runs only in the per-user daemon with that user's credentials. The privileged broker never executes it. A system-wide daemon must disable triggers. |

The per-user runtime directory is opened without following symlinks and checked
for the expected owner and mode. Stale-socket replacement verifies the existing
entry's type, owner, and inode before unlinking it. The daemon obtains peer
credentials from the connected Unix socket; a clock, root path, or trigger name
is not a bearer capability. BSER/JSON nesting, PDU bytes, result paths, result
bytes, waiters, cuts, and comparisons all have explicit limits. A semantic
result which exceeds its path budget becomes a conservative fresh result; a
malformed or oversized transport request is rejected.

Each grant has one ordering gate for response and privileged dispatch.
Ordinary projection runs without it; the query takes a shared response phase
only for its final fenced authorization check and bounded nonblocking write.
The frame has a fixed byte cap/deadline; on timeout the daemon closes the
connection so a partial frame is unusable before releasing the gate. A broker
dispatch takes the shared execution phase through the durable-start handshake
above. Revocation takes the exclusive phase, then atomically revokes the grant
and fences its admissions, query leases, operations, and triggers. It therefore
orders before an unstarted response/dispatch, after bytes already sent, or
after an already-running filesystem mutation has been exactly reconciled. A
paused projection cannot disclose paths after revocation merely because it was
admitted earlier, and revocation never pretends to cancel an ioctl already in
the kernel.

A future unprivileged delta ABI is reasonable only with fd-anchored roots and a
deliberate whole-subvolume authorization mechanism, such as a privileged-issued
watch handle. Rechecking VFS permission on every descendant would avoid the
metadata leak but would change semantics under concurrent permission changes
and discard much of the performance benefit. `CAP_DAC_READ_SEARCH` is narrower
than `CAP_SYS_ADMIN`, but it is still privilege, not an unprivileged design.

Initialize creates the watch and its first grant in the same transaction.
Concurrent callers may attach to that initialization only after passing the
same authorization policy. Creating a tracked worktree establishes an explicit
grant for the child watch; it does not assume that knowing the parent watch ID
or sharing its index grants access.

## 7. SQLite schema

Use SQLite WAL mode with one database per manager store:

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA busy_timeout = 30000;
```

Btrfs names and paths are `BLOB`, never `TEXT`. UUIDs are 16-byte BLOBs. Btrfs
`u64` values are stored as 8-byte big-endian BLOBs (`U64` below), because
SQLite integers are signed. Timestamps fit in signed nanoseconds and are used
for diagnostics/leases only, never snapshot ordering. Every `*_expires_ns` and
`expires_ns` value is an absolute `CLOCK_BOOTTIME` deadline scoped to
`service_metadata.last_boot_id`; a boot-ID change expires and fences all such
leases/admissions before service. Other `*_ns` columns are Unix-time
diagnostics and are never compared for correctness.

The following is the normative logical schema; migrations may add surrogate
columns and indexes without changing the invariants.

```sql
CREATE TABLE service_metadata (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    store_uuid      BLOB NOT NULL UNIQUE CHECK (length(store_uuid) = 16),
    clock_hmac_key  BLOB NOT NULL CHECK (length(clock_hmac_key) = 32),
    clock_format_version INTEGER NOT NULL CHECK (clock_format_version > 0),
    last_boot_id    BLOB NOT NULL CHECK (length(last_boot_id) = 16),
    created_ns      INTEGER NOT NULL
);

CREATE TABLE filesystems (
    id              INTEGER PRIMARY KEY,
    fs_uuid         BLOB NOT NULL UNIQUE CHECK (length(fs_uuid) = 16)
);

CREATE TABLE topology_leases (
    filesystem_id   INTEGER PRIMARY KEY REFERENCES filesystems(id),
    lease_owner     BLOB,
    lease_fence     INTEGER NOT NULL DEFAULT 0,
    lease_expires_ns INTEGER
);

CREATE TABLE snapshots (
    id              INTEGER PRIMARY KEY,
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    subvol_uuid     BLOB NOT NULL CHECK (length(subvol_uuid) = 16),
    parent_uuid     BLOB CHECK (parent_uuid IS NULL OR length(parent_uuid) = 16),
    received_uuid   BLOB CHECK (received_uuid IS NULL OR length(received_uuid) = 16),
    root_id         BLOB NOT NULL CHECK (length(root_id) = 8),
    ctransid        BLOB NOT NULL CHECK (length(ctransid) = 8),
    otransid        BLOB NOT NULL CHECK (length(otransid) = 8),
    path            BLOB NOT NULL,
    readonly        INTEGER NOT NULL CHECK (readonly = 1),
    physical_state  TEXT NOT NULL CHECK
                    (physical_state IN
                     ('creating', 'present', 'deleting', 'deleted', 'lost')),
    created_ns      INTEGER NOT NULL,
    deleted_ns      INTEGER,
    UNIQUE (filesystem_id, subvol_uuid)
);

CREATE UNIQUE INDEX snapshots_live_path
ON snapshots(filesystem_id, path)
WHERE physical_state IN ('creating', 'present', 'deleting');

CREATE TABLE revisions (
    id              INTEGER PRIMARY KEY,
    snapshot_id     INTEGER NOT NULL UNIQUE REFERENCES snapshots(id),
    storage_base_revision_id INTEGER REFERENCES revisions(id),
    provenance_comparison_id INTEGER REFERENCES comparisons(id),
    delta_depth     INTEGER NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('building', 'ready', 'failed')),
    builder_owner   BLOB,
    builder_fence   INTEGER NOT NULL,
    builder_expires_ns INTEGER,
    object_count    INTEGER,
    ref_count       INTEGER,
    state_hash      BLOB,
    single_owner_uid BLOB CHECK
                    (single_owner_uid IS NULL OR length(single_owner_uid) = 8),
    privileged_metadata_count INTEGER,
    security_state_hash BLOB CHECK
                    (security_state_hash IS NULL OR length(security_state_hash) = 32),
    owner_cardinality INTEGER,
    owner_uid_xor  BLOB CHECK
                    (owner_uid_xor IS NULL OR length(owner_uid_xor) = 8),
    summary_version INTEGER NOT NULL CHECK (summary_version IN (1, 2)),
    created_ns      INTEGER NOT NULL,
    CHECK ((summary_version = 1)
        OR (summary_version = 2 AND owner_cardinality > 0
            AND owner_uid_xor IS NOT NULL))
);

-- A checkpoint is a full materialization of an immutable revision. The first
-- revision is a checkpoint; later revisions may gain one during compaction.
CREATE TABLE revision_checkpoints (
    revision_id     INTEGER PRIMARY KEY REFERENCES revisions(id),
    state           TEXT NOT NULL CHECK (state IN ('building', 'ready')),
    builder_owner   BLOB,
    builder_fence   INTEGER NOT NULL,
    builder_expires_ns INTEGER,
    object_count    INTEGER,
    ref_count       INTEGER,
    state_hash      BLOB
);

CREATE TABLE checkpoint_objects (
    revision_id     INTEGER NOT NULL REFERENCES revision_checkpoints(revision_id),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    generation      BLOB NOT NULL CHECK (length(generation) = 8),
    mode            INTEGER NOT NULL,
    nlink           INTEGER NOT NULL,
    uid             BLOB NOT NULL CHECK (length(uid) = 8),
    gid             BLOB NOT NULL CHECK (length(gid) = 8),
    rdev            BLOB NOT NULL CHECK (length(rdev) = 8),
    privilege_flags INTEGER NOT NULL,
    security_xattr_hash BLOB NOT NULL CHECK (length(security_xattr_hash) = 32),
    PRIMARY KEY (revision_id, ino)
) WITHOUT ROWID;

CREATE TABLE checkpoint_refs (
    revision_id     INTEGER NOT NULL REFERENCES revision_checkpoints(revision_id),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    parent_ino      BLOB NOT NULL CHECK (length(parent_ino) = 8),
    name            BLOB NOT NULL,
    PRIMARY KEY (revision_id, ino, parent_ino, name)
) WITHOUT ROWID;

CREATE INDEX checkpoint_refs_by_parent
ON checkpoint_refs(revision_id, parent_ino, name, ino);

CREATE UNIQUE INDEX checkpoint_one_child_per_name
ON checkpoint_refs(revision_id, parent_ino, name);

CREATE TABLE checkpoint_owner_counts (
    revision_id     INTEGER NOT NULL REFERENCES revision_checkpoints(revision_id),
    uid             BLOB NOT NULL CHECK (length(uid) = 8),
    object_count    INTEGER NOT NULL CHECK (object_count > 0),
    PRIMARY KEY (revision_id, uid)
) WITHOUT ROWID;

-- Target-state overrides relative to storage_base_revision_id.
CREATE TABLE object_overrides (
    revision_id     INTEGER NOT NULL REFERENCES revisions(id),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    present         INTEGER NOT NULL CHECK (present IN (0, 1)),
    generation      BLOB CHECK (generation IS NULL OR length(generation) = 8),
    mode            INTEGER,
    nlink           INTEGER,
    uid             BLOB CHECK (uid IS NULL OR length(uid) = 8),
    gid             BLOB CHECK (gid IS NULL OR length(gid) = 8),
    rdev            BLOB CHECK (rdev IS NULL OR length(rdev) = 8),
    privilege_flags INTEGER,
    security_xattr_hash BLOB CHECK
                    (security_xattr_hash IS NULL
                     OR length(security_xattr_hash) = 32),
    PRIMARY KEY (revision_id, ino),
    CHECK ((present = 0 AND generation IS NULL AND mode IS NULL AND nlink IS NULL
                         AND uid IS NULL AND gid IS NULL AND rdev IS NULL
                         AND privilege_flags IS NULL
                         AND security_xattr_hash IS NULL)
        OR (present = 1 AND generation IS NOT NULL
                        AND mode IS NOT NULL AND nlink IS NOT NULL
                        AND uid IS NOT NULL AND gid IS NOT NULL
                        AND rdev IS NOT NULL AND privilege_flags IS NOT NULL
                        AND security_xattr_hash IS NOT NULL))
) WITHOUT ROWID;

CREATE TABLE ref_overrides (
    revision_id     INTEGER NOT NULL REFERENCES revisions(id),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    parent_ino      BLOB NOT NULL CHECK (length(parent_ino) = 8),
    name            BLOB NOT NULL,
    present         INTEGER NOT NULL CHECK (present IN (0, 1)),
    PRIMARY KEY (revision_id, ino, parent_ino, name)
) WITHOUT ROWID;

CREATE INDEX ref_overrides_by_parent
ON ref_overrides(revision_id, parent_ino, name, ino);

-- Absolute per-UID counts relative to storage_base_revision_id. Only UIDs
-- touched by this delta need rows; zero is retained to mask an inherited UID.
CREATE TABLE owner_count_overrides (
    revision_id     INTEGER NOT NULL REFERENCES revisions(id),
    uid             BLOB NOT NULL CHECK (length(uid) = 8),
    object_count    INTEGER NOT NULL CHECK (object_count >= 0),
    PRIMARY KEY (revision_id, uid)
) WITHOUT ROWID;

-- summary_version 2 makes state_hash and security_state_hash incrementally
-- composable: each is the XOR accumulator of domain-separated SHA-256 entry
-- digests keyed by inode or reference identity. Replacing an entry XORs out
-- its old digest and XORs in its new digest. Object/reference counts and the
-- privileged-metadata count use checked signed deltas. Owner cardinality is
-- exact, not inferred from single_owner_uid: checkpoint rows contain every
-- positive UID count, overlays contain absolute counts only for touched UIDs,
-- and zero masks an inherited owner. owner_uid_xor changes only on a
-- zero/nonzero transition; with exact cardinality it recovers the sole UID.
-- Version-1 summaries are checkpointed before another delta is published.

CREATE TABLE comparisons (
    id              INTEGER PRIMARY KEY,
    from_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    to_snapshot_id  INTEGER NOT NULL REFERENCES snapshots(id),
    comparison_kind TEXT NOT NULL CHECK
                    (comparison_kind IN ('incremental', 'full_fresh')),
    algorithm_version INTEGER NOT NULL,
    state           TEXT NOT NULL CHECK
                    (state IN ('claimed', 'manifest_ready', 'index_ready', 'failed')),
    lease_owner     BLOB,
    lease_fence     INTEGER NOT NULL DEFAULT 0,
    lease_expires_ns INTEGER,
    manifest_hash   BLOB,
    raw_ref_adds    INTEGER,
    raw_ref_deletes INTEGER,
    UNIQUE (from_snapshot_id, to_snapshot_id,
            comparison_kind, algorithm_version),
    UNIQUE (id, from_snapshot_id, to_snapshot_id)
);

CREATE TABLE comparison_objects (
    comparison_id   INTEGER NOT NULL REFERENCES comparisons(id),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    old_generation  BLOB CHECK (old_generation IS NULL OR length(old_generation) = 8),
    new_generation  BLOB CHECK (new_generation IS NULL OR length(new_generation) = 8),
    change_mask     INTEGER NOT NULL,
    PRIMARY KEY (comparison_id, ino)
) WITHOUT ROWID;

CREATE TABLE comparison_refs (
    comparison_id   INTEGER NOT NULL REFERENCES comparisons(id),
    operation       INTEGER NOT NULL CHECK (operation IN (-1, 1)),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    parent_ino      BLOB NOT NULL CHECK (length(parent_ino) = 8),
    name            BLOB NOT NULL,
    PRIMARY KEY (comparison_id, operation, ino, parent_ino, name)
) WITHOUT ROWID;

CREATE TABLE change_events (
    comparison_id   INTEGER NOT NULL REFERENCES comparisons(id),
    ordinal         INTEGER NOT NULL,
    event_kind      TEXT NOT NULL,
    ino             BLOB CHECK (ino IS NULL OR length(ino) = 8),
    old_generation  BLOB CHECK (old_generation IS NULL OR length(old_generation) = 8),
    new_generation  BLOB CHECK (new_generation IS NULL OR length(new_generation) = 8),
    change_mask     INTEGER NOT NULL,
    old_path        BLOB,
    new_path        BLOB,
    PRIMARY KEY (comparison_id, ordinal)
) WITHOUT ROWID;

CREATE TABLE watches (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    live_subvol_uuid BLOB NOT NULL CHECK (length(live_subvol_uuid) = 16),
    live_path       BLOB NOT NULL,
    indexed_revision_id INTEGER REFERENCES revisions(id),
    indexed_seq     INTEGER,
    last_cut_snapshot_id INTEGER REFERENCES snapshots(id),
    last_cut_seq    INTEGER,
    cut_owner       BLOB,
    cut_fence       INTEGER NOT NULL DEFAULT 0,
    cut_expires_ns  INTEGER,
    clock_epoch     BLOB NOT NULL CHECK (length(clock_epoch) = 16),
    replay_floor_seq INTEGER,
    fsmonitor_owner_grant_id BLOB
                    CHECK (fsmonitor_owner_grant_id IS NULL
                           OR length(fsmonitor_owner_grant_id) = 16),
    fsmonitor_root  BLOB,
    mount_ns_dev    BLOB CHECK (mount_ns_dev IS NULL OR length(mount_ns_dev) = 8),
    mount_ns_ino    BLOB CHECK (mount_ns_ino IS NULL OR length(mount_ns_ino) = 8),
    view_root_dev   BLOB CHECK (view_root_dev IS NULL OR length(view_root_dev) = 8),
    view_root_ino   BLOB CHECK (view_root_ino IS NULL OR length(view_root_ino) = 8),
    view_root_mnt_id BLOB CHECK
                    (view_root_mnt_id IS NULL OR length(view_root_mnt_id) = 8),
    view_monitor_session_id BLOB CHECK
                    (view_monitor_session_id IS NULL
                     OR length(view_monitor_session_id) = 16),
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    guard_head_seq  INTEGER,
    guard_replay_floor_seq INTEGER,
    fsmonitor_state TEXT NOT NULL CHECK
                    (fsmonitor_state IN
                     ('disabled', 'snapshot_only', 'guard_arming',
                      'guard_active', 'guard_gapped')),
    state           TEXT NOT NULL CHECK
                    (state IN ('initializing', 'active', 'blocked', 'deleted')),
    CHECK (
        (state = 'initializing'
         AND indexed_revision_id IS NULL AND indexed_seq IS NULL
         AND last_cut_snapshot_id IS NULL AND last_cut_seq IS NULL
         AND replay_floor_seq IS NULL)
        OR
        (state IN ('active', 'blocked')
         AND indexed_revision_id IS NOT NULL AND indexed_seq IS NOT NULL
         AND last_cut_snapshot_id IS NOT NULL AND last_cut_seq IS NOT NULL
         AND replay_floor_seq IS NOT NULL
         AND replay_floor_seq <= indexed_seq
         AND indexed_seq <= last_cut_seq)
        OR
        (state = 'deleted'
         AND indexed_revision_id IS NULL AND indexed_seq IS NULL
         AND last_cut_snapshot_id IS NULL AND last_cut_seq IS NULL
         AND replay_floor_seq IS NULL)
    ),
    CHECK (
        (fsmonitor_state = 'disabled'
         AND fsmonitor_owner_grant_id IS NULL AND fsmonitor_root IS NULL
         AND mount_ns_dev IS NULL AND mount_ns_ino IS NULL
         AND view_root_dev IS NULL AND view_root_ino IS NULL
         AND view_root_mnt_id IS NULL
         AND view_monitor_session_id IS NULL
         AND guard_epoch IS NULL AND guard_head_seq IS NULL
         AND guard_replay_floor_seq IS NULL)
        OR
        (fsmonitor_state = 'snapshot_only'
         AND state = 'active'
         AND fsmonitor_owner_grant_id IS NOT NULL
         AND fsmonitor_root IS NOT NULL
         AND mount_ns_dev IS NOT NULL AND mount_ns_ino IS NOT NULL
         AND view_root_dev IS NOT NULL AND view_root_ino IS NOT NULL
         AND view_root_mnt_id IS NOT NULL
         AND view_monitor_session_id IS NOT NULL
         AND guard_epoch IS NULL AND guard_head_seq IS NULL
         AND guard_replay_floor_seq IS NULL)
        OR
        (fsmonitor_state IN
         ('guard_arming', 'guard_active', 'guard_gapped')
         AND state = 'active'
         AND fsmonitor_owner_grant_id IS NOT NULL
         AND fsmonitor_root IS NOT NULL
         AND mount_ns_dev IS NOT NULL AND mount_ns_ino IS NOT NULL
         AND view_root_dev IS NOT NULL AND view_root_ino IS NOT NULL
         AND view_root_mnt_id IS NOT NULL
         AND view_monitor_session_id IS NOT NULL
         AND guard_epoch IS NOT NULL AND guard_head_seq IS NOT NULL
         AND guard_replay_floor_seq IS NOT NULL
         AND guard_replay_floor_seq <= guard_head_seq)
    ),
    FOREIGN KEY (fsmonitor_owner_grant_id, id)
        REFERENCES watch_grants(id, watch_id)
);

CREATE UNIQUE INDEX watches_live_subvolume
ON watches(filesystem_id, live_subvol_uuid)
WHERE state IN ('initializing', 'active', 'blocked');

CREATE TABLE watch_grants (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    principal_kind  TEXT NOT NULL CHECK
                    (principal_kind IN ('uid', 'service')),
    principal_id    BLOB NOT NULL,
    -- 0x01 READ, 0x02 CUT, 0x04 WORKTREE, 0x08 TRIGGER,
    -- 0x10 RETAIN, 0x20 ADMIN. Unknown bits are rejected.
    permissions     INTEGER NOT NULL CHECK
                    (permissions > 0 AND (permissions & ~63) = 0),
    state           TEXT NOT NULL CHECK (state IN ('active', 'revoked')),
    created_ns      INTEGER NOT NULL,
    revoked_ns      INTEGER,
    CHECK ((state = 'active' AND revoked_ns IS NULL)
        OR (state = 'revoked' AND revoked_ns IS NOT NULL)),
    UNIQUE (id, watch_id)
);

CREATE UNIQUE INDEX watch_grants_one_active_principal
ON watch_grants(watch_id, principal_kind, principal_id)
WHERE state = 'active';

-- Required whenever the grant has WORKTREE (0x04). V1 authorizes one entire
-- destination subvolume by identity; root_ino must be that subvolume's inode
-- 256. Narrower ordinary-directory policies need kernel/LSM path-beneath
-- enforcement and are not approximated with a racy ancestry check.
CREATE TABLE worktree_grant_policies (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    grant_id        BLOB NOT NULL UNIQUE REFERENCES watch_grants(id),
    destination_filesystem_id INTEGER NOT NULL REFERENCES filesystems(id),
    destination_root_subvol_uuid BLOB NOT NULL
                    CHECK (length(destination_root_subvol_uuid) = 16),
    destination_root_path BLOB NOT NULL,
    destination_root_ino BLOB NOT NULL CHECK (length(destination_root_ino) = 8),
    destination_root_generation BLOB NOT NULL
                    CHECK (length(destination_root_generation) = 8),
    metadata_policy TEXT NOT NULL CHECK
                    (metadata_policy IN
                     ('sanitized-private-user-tree',
                      'admin-trusted-preserve')),
    allow_idmapped  INTEGER NOT NULL DEFAULT 0 CHECK (allow_idmapped IN (0, 1)),
    policy_hash     BLOB NOT NULL CHECK (length(policy_hash) = 32),
    created_ns      INTEGER NOT NULL,
    UNIQUE (id, grant_id)
);

-- Conservative mutation hints between immutable cuts. The guard producer
-- emits two path rows for a rename. A NULL path is permitted only for a
-- whole-tree invalidation marker.
CREATE TABLE mutation_events (
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    guard_epoch     BLOB NOT NULL CHECK (length(guard_epoch) = 16),
    sequence        INTEGER NOT NULL CHECK (sequence >= 0),
    event_kind      TEXT NOT NULL CHECK
                    (event_kind IN ('path', 'directory-prefix',
                                    'object', 'full-invalidation')),
    path            BLOB,
    ino             BLOB CHECK (ino IS NULL OR length(ino) = 8),
    generation      BLOB CHECK (generation IS NULL OR length(generation) = 8),
    observed_ns     INTEGER NOT NULL,
    PRIMARY KEY (watch_id, guard_epoch, sequence),
    CHECK ((event_kind = 'full-invalidation' AND path IS NULL)
        OR (event_kind != 'full-invalidation' AND path IS NOT NULL))
) WITHOUT ROWID;

-- V1 persists only jj's fixed background-snapshot trigger. The kind columns
-- reconstruct the exact argv/expression/null-redirection response; arbitrary
-- commands and redirection paths are deliberately not stored or executed.
CREATE TABLE watchman_triggers (
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    name            TEXT NOT NULL CHECK (name = 'jj-background-monitor'),
    owner_grant_id  BLOB NOT NULL CHECK (length(owner_grant_id) = 16),
    command_kind    TEXT NOT NULL CHECK (command_kind = 'jj-snapshot-v1'),
    expression_kind TEXT NOT NULL CHECK (expression_kind = 'exclude-git-jj-v1'),
    state           TEXT NOT NULL CHECK (state IN ('active', 'deleting')),
    last_evaluated_seq INTEGER,
    pending_through_seq INTEGER,
    run_owner       BLOB,
    run_fence       INTEGER NOT NULL DEFAULT 0,
    run_expires_ns  INTEGER,
    PRIMARY KEY (watch_id, owner_grant_id, name),
    FOREIGN KEY (owner_grant_id, watch_id)
        REFERENCES watch_grants(id, watch_id),
    CHECK (last_evaluated_seq IS NULL
        OR pending_through_seq IS NULL
        OR last_evaluated_seq <= pending_through_seq),
    CHECK ((run_owner IS NULL) = (run_expires_ns IS NULL))
);

CREATE TABLE operations (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    kind            TEXT NOT NULL CHECK
                    (kind IN ('initialize', 'cut', 'worktree')),
    state           TEXT NOT NULL CHECK
                    (state IN ('planned', 'fs_started', 'fs_created', 'uuid_recorded',
                               'manifest_ready', 'index_committed',
                               'awaiting_destination', 'done', 'failed')),
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    sequence        INTEGER,
    source_subvol_uuid BLOB NOT NULL CHECK (length(source_subvol_uuid) = 16),
    base_snapshot_id INTEGER REFERENCES snapshots(id),
    expected_parent_uuid BLOB NOT NULL CHECK (length(expected_parent_uuid) = 16),
    requested_readonly INTEGER NOT NULL CHECK (requested_readonly IN (0, 1)),
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    guard_sequence  INTEGER,
    requester_uid   INTEGER NOT NULL,
    requester_gid   INTEGER NOT NULL,
    authorization_id BLOB NOT NULL CHECK (length(authorization_id) = 16),
    worktree_policy_id BLOB,
    reserved_path   BLOB NOT NULL,
    final_path      BLOB,  -- diagnostic only; never a recovery authority
    destination_parent_subvol_uuid BLOB CHECK
                    (destination_parent_subvol_uuid IS NULL
                     OR length(destination_parent_subvol_uuid) = 16),
    destination_parent_ino BLOB CHECK
                    (destination_parent_ino IS NULL
                     OR length(destination_parent_ino) = 8),
    destination_parent_generation BLOB CHECK
                    (destination_parent_generation IS NULL
                     OR length(destination_parent_generation) = 8),
    destination_name BLOB,
    destination_reservation_name BLOB,
    destination_reservation_ino BLOB CHECK
                    (destination_reservation_ino IS NULL
                     OR length(destination_reservation_ino) = 8),
    destination_reservation_generation BLOB CHECK
                    (destination_reservation_generation IS NULL
                     OR length(destination_reservation_generation) = 8),
    destination_reservation_nonce BLOB CHECK
                    (destination_reservation_nonce IS NULL
                     OR length(destination_reservation_nonce) = 32),
    discovered_uuid BLOB CHECK
                    (discovered_uuid IS NULL OR length(discovered_uuid) = 16),
    lease_owner     BLOB,
    lease_fence     INTEGER NOT NULL,
    lease_expires_ns INTEGER,
    error           TEXT,
    updated_ns      INTEGER NOT NULL,
    UNIQUE (watch_id, sequence),
    UNIQUE (id, watch_id),
    UNIQUE (id, watch_id, sequence),
    FOREIGN KEY (authorization_id, watch_id)
        REFERENCES watch_grants(id, watch_id),
    FOREIGN KEY (worktree_policy_id, authorization_id)
        REFERENCES worktree_grant_policies(id, grant_id),
    CHECK ((guard_epoch IS NULL) = (guard_sequence IS NULL)),
    CHECK (kind != 'worktree'
        OR (worktree_policy_id IS NOT NULL
            AND final_path IS NOT NULL
            AND destination_parent_subvol_uuid IS NOT NULL
            AND destination_parent_ino IS NOT NULL
            AND destination_parent_generation IS NOT NULL
            AND destination_name IS NOT NULL
            AND destination_reservation_name IS NOT NULL
            AND destination_reservation_ino IS NOT NULL
            AND destination_reservation_generation IS NOT NULL
            AND destination_reservation_nonce IS NOT NULL)),
    CHECK (kind = 'worktree' OR worktree_policy_id IS NULL)
);

CREATE UNIQUE INDEX operations_active_reserved_path
ON operations(filesystem_id, reserved_path)
WHERE state NOT IN ('done', 'failed');

-- A compatibility request joins a cut only through this writer-serialized
-- record. This closes the read/check versus fs_started race.
CREATE TABLE cut_admissions (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    operation_id    BLOB NOT NULL,
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    authorization_id BLOB NOT NULL CHECK (length(authorization_id) = 16),
    requester_session_id BLOB NOT NULL
                    CHECK (length(requester_session_id) = 16),
    request_kind    TEXT NOT NULL CHECK
                    (request_kind IN ('clock', 'query', 'trigger')),
    state           TEXT NOT NULL CHECK
                    (state IN ('waiting', 'fulfilled', 'abandoned')),
    admitted_ns     INTEGER NOT NULL,
    expires_ns      INTEGER NOT NULL,
    FOREIGN KEY (operation_id, watch_id)
        REFERENCES operations(id, watch_id),
    FOREIGN KEY (authorization_id, watch_id)
        REFERENCES watch_grants(id, watch_id)
);

CREATE TABLE watch_cuts (
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    sequence        INTEGER NOT NULL,
    operation_id    BLOB NOT NULL UNIQUE,
    base_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    target_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    comparison_id   INTEGER REFERENCES comparisons(id),
    comparison_from_snapshot_id INTEGER REFERENCES snapshots(id),
    state           TEXT NOT NULL CHECK
                    (state IN ('created', 'comparing', 'ready', 'failed')),
    fresh_instance  INTEGER NOT NULL DEFAULT 0 CHECK (fresh_instance IN (0, 1)),
    PRIMARY KEY (watch_id, sequence),
    UNIQUE (watch_id, target_snapshot_id),
    UNIQUE (watch_id, sequence, target_snapshot_id, operation_id),
    FOREIGN KEY (operation_id, watch_id, sequence)
        REFERENCES operations(id, watch_id, sequence),
    FOREIGN KEY (comparison_id, comparison_from_snapshot_id, target_snapshot_id)
        REFERENCES comparisons(id, from_snapshot_id, to_snapshot_id)
) WITHOUT ROWID;

CREATE INDEX watch_cuts_ready_range
ON watch_cuts(watch_id, sequence, comparison_id)
WHERE state = 'ready';

-- One committed external-clock boundary per fsmonitor-visible cut. Sequence 0
-- is valid here even though it has no watch_cuts row. Guard fields are an
-- optional precision cursor, not part of the coarse dirty-witness proof.
CREATE TABLE fsmonitor_boundaries (
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    cut_sequence    INTEGER NOT NULL CHECK (cut_sequence >= 0),
    target_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    boundary_kind   TEXT NOT NULL CHECK
                    (boundary_kind IN ('cut', 'proved_worktree_seed')),
    cut_operation_id BLOB,
    seed_worktree_id BLOB REFERENCES worktrees(id),
    clock_epoch     BLOB NOT NULL CHECK (length(clock_epoch) = 16),
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    guard_sequence  INTEGER CHECK (guard_sequence IS NULL OR guard_sequence >= 0),
    guard_complete  INTEGER NOT NULL CHECK (guard_complete IN (0, 1)),
    PRIMARY KEY (watch_id, cut_sequence),
    UNIQUE (watch_id, target_snapshot_id),
    FOREIGN KEY (watch_id, cut_sequence, target_snapshot_id, cut_operation_id)
        REFERENCES watch_cuts(watch_id, sequence,
                              target_snapshot_id, operation_id),
    CHECK ((boundary_kind = 'cut' AND cut_sequence > 0
            AND cut_operation_id IS NOT NULL
            AND seed_worktree_id IS NULL)
        OR (boundary_kind = 'proved_worktree_seed' AND cut_sequence = 0
            AND cut_operation_id IS NULL
            AND seed_worktree_id IS NOT NULL)),
    CHECK ((guard_complete = 0
            AND guard_epoch IS NULL AND guard_sequence IS NULL)
        OR (guard_complete = 1
            AND guard_epoch IS NOT NULL AND guard_sequence IS NOT NULL))
) WITHOUT ROWID;

CREATE TABLE query_leases (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    authorization_id BLOB NOT NULL CHECK (length(authorization_id) = 16),
    clock_epoch     BLOB NOT NULL CHECK (length(clock_epoch) = 16),
    from_cut_sequence INTEGER,
    to_cut_sequence INTEGER NOT NULL,
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    from_guard_sequence INTEGER,
    to_guard_sequence INTEGER,
    lease_owner     BLOB NOT NULL,
    lease_fence     INTEGER NOT NULL,
    lease_expires_ns INTEGER NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('active', 'released')),
    FOREIGN KEY (authorization_id, watch_id)
        REFERENCES watch_grants(id, watch_id),
    CHECK (from_cut_sequence IS NULL
        OR from_cut_sequence <= to_cut_sequence),
    CHECK (from_guard_sequence IS NULL
        OR from_guard_sequence <= to_guard_sequence),
    CHECK ((guard_epoch IS NULL
            AND from_guard_sequence IS NULL AND to_guard_sequence IS NULL)
        OR (guard_epoch IS NOT NULL
            AND from_guard_sequence IS NOT NULL
            AND to_guard_sequence IS NOT NULL))
);

CREATE TABLE query_revision_pins (
    query_id        BLOB NOT NULL REFERENCES query_leases(id),
    revision_id     INTEGER NOT NULL REFERENCES revisions(id),
    PRIMARY KEY (query_id, revision_id)
) WITHOUT ROWID;

CREATE TABLE query_comparison_pins (
    query_id        BLOB NOT NULL REFERENCES query_leases(id),
    comparison_id   INTEGER NOT NULL REFERENCES comparisons(id),
    PRIMARY KEY (query_id, comparison_id)
) WITHOUT ROWID;

CREATE TABLE worktrees (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    subvol_uuid     BLOB CHECK (subvol_uuid IS NULL OR length(subvol_uuid) = 16),
    path            BLOB NOT NULL,
    seed_revision_id INTEGER REFERENCES revisions(id),
    watch_id        BLOB REFERENCES watches(id),
    operation_id    BLOB NOT NULL UNIQUE REFERENCES operations(id),
    state           TEXT NOT NULL CHECK
                    (state IN ('creating', 'present', 'deleting', 'deleted')),
    UNIQUE (filesystem_id, subvol_uuid),
    CHECK ((state = 'creating' AND seed_revision_id IS NOT NULL)
        OR (state IN ('present', 'deleting')
            AND seed_revision_id IS NOT NULL AND subvol_uuid IS NOT NULL)
        OR (state = 'deleted' AND seed_revision_id IS NULL))
);

CREATE UNIQUE INDEX worktrees_live_path
ON worktrees(filesystem_id, path)
WHERE state IN ('creating', 'present', 'deleting');

-- Physical GC is manager policy, not an unfenced ioctl issued directly from a
-- caller request. Caller ADMIN operations may change retention/watch state;
-- this independent durable intent owns the eventual privileged deletion.
CREATE TABLE snapshot_delete_operations (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    snapshot_id     INTEGER NOT NULL UNIQUE REFERENCES snapshots(id),
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    state           TEXT NOT NULL CHECK
                    (state IN ('planned', 'fs_started', 'fs_deleted',
                               'delete_durable', 'done', 'failed')),
    lease_owner     BLOB,
    lease_fence     INTEGER NOT NULL DEFAULT 0,
    lease_expires_ns INTEGER,
    error           TEXT,
    updated_ns      INTEGER NOT NULL,
    UNIQUE (id, snapshot_id)
);

-- Caller-controlled retention is revocable/expiring authorization state, not
-- an untyped permanent pin. Internal topology/job pins remain separate.
CREATE TABLE retention_leases (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    authorization_id BLOB NOT NULL CHECK (length(authorization_id) = 16),
    snapshot_id     INTEGER NOT NULL REFERENCES snapshots(id),
    state           TEXT NOT NULL CHECK
                    (state IN ('active', 'released', 'revoked', 'expired')),
    lease_fence     INTEGER NOT NULL DEFAULT 0,
    expires_ns      INTEGER NOT NULL,
    created_ns      INTEGER NOT NULL,
    FOREIGN KEY (authorization_id, watch_id)
        REFERENCES watch_grants(id, watch_id),
    UNIQUE (id, snapshot_id)
);

CREATE TABLE snapshot_pins (
    snapshot_id     INTEGER NOT NULL REFERENCES snapshots(id),
    owner_kind      TEXT NOT NULL CHECK
                    (owner_kind IN ('watch-indexed-head', 'watch-last-cut',
                                    'operation', 'comparison',
                                    'retention-lease')),
    owner_id        BLOB NOT NULL,
    reason          TEXT NOT NULL,
    PRIMARY KEY (snapshot_id, owner_kind, owner_id, reason)
) WITHOUT ROWID;

CREATE TRIGGER snapshot_pins_only_present
BEFORE INSERT ON snapshot_pins
WHEN (SELECT physical_state FROM snapshots WHERE id = NEW.snapshot_id)
     IS NOT 'present'
BEGIN
    SELECT RAISE(ABORT, 'cannot pin a non-present snapshot');
END;
```

The root-owned broker journal is a separate SQLite database in a directory the
manager cannot rename or replace. Its minimal logical schema is:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;

CREATE TABLE broker_receipts (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    manager_store_uuid BLOB NOT NULL CHECK (length(manager_store_uuid) = 16),
    manager_session_id BLOB NOT NULL CHECK (length(manager_session_id) = 16),
    operation_id    BLOB NOT NULL CHECK (length(operation_id) = 16),
    operation_fence INTEGER NOT NULL,
    effect_kind     TEXT NOT NULL CHECK
                    (effect_kind IN ('snapshot-create', 'worktree-rename',
                                     'snapshot-delete')),
    request_hash    BLOB NOT NULL CHECK (length(request_hash) = 32),
    filesystem_uuid BLOB NOT NULL CHECK (length(filesystem_uuid) = 16),
    target_subvol_uuid BLOB CHECK
                    (target_subvol_uuid IS NULL
                     OR length(target_subvol_uuid) = 16),
    target_locator_hash BLOB NOT NULL CHECK (length(target_locator_hash) = 32),
    state           TEXT NOT NULL CHECK
                    (state IN ('running', 'completed',
                               'failed-before-effect', 'needs-reconcile')),
    result_hash     BLOB CHECK (result_hash IS NULL OR length(result_hash) = 32),
    boot_id         BLOB NOT NULL CHECK (length(boot_id) = 16),
    started_ns      INTEGER NOT NULL,
    completed_ns    INTEGER,
    UNIQUE (manager_store_uuid, operation_id, operation_fence, request_hash),
    CHECK ((state IN ('running', 'needs-reconcile') AND completed_ns IS NULL)
        OR (state IN ('completed', 'failed-before-effect')
            AND completed_ns IS NOT NULL)),
    CHECK (state != 'completed'
        OR (target_subvol_uuid IS NOT NULL AND result_hash IS NOT NULL))
);

-- Exact fixed request bytes are committed before a running receipt can be
-- inserted. They are root-owned recovery authority, not caller authorization.
CREATE TABLE broker_request_payloads (
    manager_store_uuid BLOB NOT NULL CHECK (length(manager_store_uuid) = 16),
    operation_id    BLOB NOT NULL CHECK (length(operation_id) = 16),
    operation_fence INTEGER NOT NULL,
    opcode          INTEGER NOT NULL CHECK (opcode IN (3, 5, 6)),
    payload         BLOB NOT NULL,
    payload_hash    BLOB NOT NULL CHECK (length(payload_hash) = 32),
    PRIMARY KEY (manager_store_uuid, operation_id, operation_fence)
) WITHOUT ROWID;
```

The broker assigns each authenticated manager connection a random session ID.
A recovery handshake first fences the old session, drains/rejects any request
which has not reached a durable receipt, and returns a barrier before journal
inspection; an old queued message can never start afterward. Authorization is
an in-flight RAII permit retained through the complete ioctl/receipt dispatch,
and handshakes are serialized, so the barrier cannot return while a previously
authorized dispatch is still running. The manager then rotates every
post-effect operation, cut-owner, topology, and snapshot-delete lease owner in
one writer transaction. An old process which already received a broker result
therefore loses its publication CAS; if it published first, recovery instead
observes the terminal state. The broker inserts
and durably commits `running` before entering an effectful ioctl/rename. The
request hash covers every fixed argument, authorization and
Worktree-policy generation/hash, target locator, and expected identity. It
records the generated UUID/result before `completed`. A crash which leaves
`running` becomes `needs-reconcile`; only exact filesystem inspection may then
choose `completed` or `failed-before-effect`. Privileged read-only index/delta
ioctls use bounded request IDs and manager fences but need no effect receipt,
because retrying them cannot mutate namespace state.

Ready revisions are immutable. A lookup starts at a target revision, takes the
nearest object/ref override, and follows `storage_base_revision_id` until it
reaches a ready checkpoint. Keep chains bounded (for example, 32 revisions) by
materializing a full checkpoint for an existing immutable revision. Once that
checkpoint is ready, atomically set the revision's storage base to NULL and its
depth to zero; this storage-only rewrite does not change logical contents or
provenance. `provenance_comparison_id` remains immutable.

All potentially large object, ref, event, and checkpoint builds happen in a
job-local staging SQLite file named by job ID and every relevant fencing token.
Canonical child rows are imported only in the same `BEGIN IMMEDIATE`
transaction which verifies that token and publishes the parent. A stale worker
can therefore write only its private staging file. Revision and checkpoint
leases are independent of comparison leases because two comparisons with
different sources may target the same snapshot.

For summary version 2, `state_hash` is required for every ready revision and is
the composable entry-digest accumulator defined with the schema above. A delta
updates it from only replaced objects and added/deleted references; it must not
walk the entire namespace merely to fill the column. Version-1 revisions may
have a NULL flat-state hash, but are checkpointed and upgraded before serving
as another delta base. Manifest hashes and changed-row counts are always
required.

Every ready revision does require its incrementally maintained Worktree safety
summary: `single_owner_uid` is the common owner or NULL for mixed ownership,
`privileged_metadata_count` counts any set-ID/device/capability/disallowed
security metadata, and `security_state_hash` commits to the per-object
classification. `privilege_flags` and the security-xattr digest are derived by
the privileged full-index/targeted-lookup path, not trusted from a caller. The
sanitized Worktree policy requires the common UID to equal the requester and
the count to be zero; any unknown classification is unsafe. This extra metadata
exists for clone authorization and is not exposed in jj/Git change results.

`clock_epoch`, a ready cut sequence, and its `fsmonitor_boundaries` row identify
a filesystem-monitor boundary. `replay_floor_seq` is the oldest cut clock for
which all later ready comparisons are retained. An optional guard cursor marks
a boundary from which exact namespace events may be replayed;
`guard_replay_floor_seq` is only that precision journal's floor. Losing guard
history degrades an interval to coarse dirty-witness projection and does not by
itself invalidate the cut clock. Moving either floor and deleting its history
is one transaction. `store_uuid`, watch/grant IDs, clock epoch, cut sequence,
algorithm version, target snapshot identity, and any optional guard cursor are
authenticated into an opaque ASCII clock. The HMAC prevents token tampering
but grants remain the authorization boundary.

Grant permission bits are normative: `READ=0x01`, `CUT=0x02`,
`WORKTREE=0x04`, `TRIGGER=0x08`, `RETAIN=0x10`, and `ADMIN=0x20`.
Historical read/replay requires `READ`; `Changes`, `clock`, and `query` require
`READ|CUT`; Worktree requires `READ|CUT|WORKTREE`; trigger registration and
evaluation require `READ|CUT|TRIGGER`; caller-controlled retention requires
`RETAIN`; and watch deletion or caller-directed GC requires `ADMIN`. Automatic
policy GC runs as the manager rather than borrowing a caller bit. A broker
request carries the exact authorization generation and required mask; unknown
bits and a merely overlapping mask are rejected.

Worktree policy rows are immutable authorization generations. Changing an
anchor or metadata policy requires revoking the grant and creating a new grant
and policy UUID/hash. The Worktree operation copies that exact policy ID, and
the root-owned broker receipt binds the ID/hash plus observed destination
mount, idmap, LSM-domain, and anchored-parent facts. Recovery never reevaluates
an old operation under a replacement policy.

The guard producer reserves no sequence in memory. It appends each
`mutation_events` row and advances `watches.guard_head_seq` in the same
`BEGIN IMMEDIATE` transaction, so a captured head names exactly the durable
prefix. Rename sides use separate consecutive sequences. A
`full-invalidation` row is itself replayable; a true queue/coverage gap changes
the watch to `guard_gapped`. Later cut boundaries omit a guard cursor and use
coarse projection until a new epoch is fully armed.

A clean index-manager restart in the same boot preserves cut clocks only while
the separate namespace daemon has retained both the original mountinfo fd and
every mandatory root-component binding watch and can prove
snapshot/comparison and source continuity. Restarting or losing either
mandatory monitor, or overflow/`IN_IGNORED` on the binding fd, loses evidence
of a transient external view change and therefore rotates the corresponding
clock epochs, even in the same boot. A boot-ID change,
unclean manager handoff, remount/rollback ambiguity, or source-identity
discontinuity does the same: a crash can discard both an uncommitted transient
mutation and its dirty witness while client metadata persisted elsewhere. If
only the separable optional recursive-journal producer restarts, its own
inotify fd overflows, or one of its subtree watches is lost while both mandatory
monitors survive, mark the precision guard gapped; the next query remains
correct through the persistent dirty witness but may return a fresh/full
result.

`cut_admissions` is the only way a compatibility request joins a planned cut;
the insert and the worker's `planned -> fs_started` close are competing short
writer transactions. Each row records the joining caller's authorization
generation, session, and expiry independently of the cut operation's creator.
Revocation can therefore fence precisely the affected waiters, and startup
abandons expired sessions rather than guessing from a connection which no
longer exists. Admission does not pin the caller's old cut: GC may reclaim it
while B is being produced, in which case that request degrades to a fresh
result. This is a QoS tradeoff, not a correctness failure.

`query_leases` pin every comparison and revision needed for projection and
protect their mutation-event range after the selecting read transaction ends.
The mutation range is optional; coarse projection needs only the cut/revision
pins. A long projection renews its lease before expiry and must retain a
private immutable copy of every encoded row or keep its pins until the final
response check. GC reclaims an expired query only under a new fence, and a
projector whose renewal, authorization, epoch, or fence check fails discards
its result.

Grant UUIDs are append-only authorization generations. Revocation is terminal;
reauthorizing the same principal inserts a new UUID. The revocation transaction
fences every waiting admission, active query lease, unstarted operation, and
trigger tied to the old grant, atomically terminates its retention leases and
removes their pins, disables its fsmonitor/precision state, and
rotates any replacement's clock/guard epochs. An operation with a running
broker receipt becomes revocation-pending and cannot publish to the revoked
caller; it is completed or exactly reconciled under the old fence before the
exclusive grant gate finishes and terminal cleanup runs. Revocation must never
reactivate an old row or claim to cancel a running ioctl. The gate in Section 6
orders this transaction against already encoded output and broker dispatch.
If this was the last active grant, the same transaction changes an `active`
watch to `blocked` after the running-receipt rule is satisfied; a blocked watch
may retain immutable heads for later reauthorization but serves no caller and
has no active facade. Installing a new explicit grant may revalidate identities
and return it to `active`. Revocation is never rejected merely to preserve the
active-watch invariant.

The trigger row is scheduler state, not authority to run arbitrary code. A
registration commits `pending_through_seq` before acknowledging it. A runner
claims the row with `run_fence`; completion advances `last_evaluated_seq` only
if the fence still matches. A crashed run is reclaimed conservatively and run
again. The reconstructed trigger request is the single jj request described in
Section 10; a schema migration is required before accepting another command
shape.

For an ordinary adjacent cut, `watch_cuts.comparison_from_snapshot_id` equals
`base_snapshot_id`. They differ only for the explicitly fenced Gap recovery
transaction, where the former names the actual older indexed head and the
latter retains the immediate physical-cut lineage.

Application code enforces graph invariants which cannot be expressed as simple
foreign keys across overlays:

- every ready revision belongs to exactly one immutable RO snapshot, and every
  storage base, comparison endpoint, operation base, watch head/cut, worktree,
  and boundary edge stays on that same recorded filesystem and watch branch;
- every summary-version-2 ready revision has non-NULL state/security hashes,
  positive `owner_cardinality`, an owner XOR, and
  `privileged_metadata_count`; `single_owner_uid` is non-NULL exactly when
  cardinality is one. Version-1 rows are ineligible as a delta base until
  checkpoint upgrade;
- every snapshot delete intent names the same filesystem as its exact snapshot,
  and only that intent/fence may advance the snapshot through `deleting`;
- a revision's storage base, if present, is a ready revision on that filesystem;
- the root inode has no parent reference;
- every other indexed object is reachable and has at least one reference;
- every non-root directory has exactly one effective reference, the effective
  parent/name map has at most one child, and directory ancestry is acyclic.
  Overlay publication validates inherited-plus-override effective state, since
  an index on override additions alone cannot detect a conflict with an
  inherited name;
- every reference names a present child and present directory parent;
- every active watch has at least one active grant; a ready cut's
  comparison ID/from/target tuple matches its comparison row;
- every grant carrying `WORKTREE` has exactly one immutable anchored
  `worktree_grant_policies` row, no grant without that bit has one, and every
  Worktree operation's policy belongs to its exact `authorization_id`; every
  request revalidates the anchor and runtime mount/idmap/LSM policy, and
  `sanitized-private-user-tree` always has `allow_idmapped=0`;
- every sequence above a watch's replay floor through its indexed head is a
  complete ready cut with no unmarked gap;
- every `cut` fsmonitor boundary names its exact ready
  `(watch, sequence, snapshot, operation)` tuple. A `proved_worktree_seed`
  boundary instead names the exact present Worktree row whose child watch,
  seed revision, and seed snapshot match it; no Initialize seed is eligible.
  An optional guard cursor is in the boundary's complete precision epoch. The
  single fsmonitor owner and every active trigger refer to active grants for
  that watch;
- every active query lease pins every comparison/revision it may read and
  prevents guard-event GC through any recorded range, and its authorization
  generation and clock epoch still match before response; and
- every active caller `retention_leases` row belongs to an active grant with
  `RETAIN`, names a snapshot on that watch branch, and has exactly one matching
  `snapshot_pins(owner_kind='retention-lease', owner_id=lease.id)` row; terminal
  or expired retention rows have none; and
- a writable worktree's seed revision describes only its creation state, not
  its later mutable contents.

## 8. Transaction protocols

No SQLite write transaction remains open during a Btrfs ioctl, a tree walk, or
path expansion. SQLite has one writer; WAL allows concurrent readers while the
service queues writer transactions and uses `BEGIN IMMEDIATE`. Intent, lease,
and incremental publication transactions should be short. The initial full
checkpoint import can be a longer bulk writer transaction, but it does not
block WAL readers; independent kernel walks and comparisons still run in
parallel.

### 8.1 Initialize(source)

1. Open the supplied path as a subvolume-root fd. Read FSID and subvolume info,
   verify inode 256/root status, authorize the whole watch, and reject nested
   subvolumes. Also reject the top-level root or a source which contains the
   configured manager store.
2. Acquire the per-filesystem topology lease. Recheck that the source contains
   neither the manager store nor any non-deleted worktree reservation.
   In one short transaction, insert `watches(state='initializing')` to reserve
   `(filesystem_id, live_subvol_uuid)`, insert its first authorized
   `watch_grants` row, generate its clock epoch, allocate operation ID and
   sequence 0, and insert the `operations(state='planned')` row with its protected path,
   source/expected-parent UUID, FSID, requested RO flag, authorization, lease,
   and fence. Release the topology lease only after these reservations are
   visible. The target UUID is still unknown. A concurrent Initialize may
   attach only after authorization and then reuses this idempotent operation;
   it cannot perform a second scan and race only at publication.
3. Fence the operation from `planned` to `fs_started`, then create a read-only
   snapshot `S0`. Reopen it and verify FSID, new UUID, `parent_uuid`, `ctransid`,
   path, and read-only flag. Insert the snapshot row, set
   `operations.discovered_uuid`, and pin S0 under the operation in the same
   transaction.
4. Validate S0 itself has no nested subvolume boundaries. Produce a complete
   OBJECT+REF manifest for S0 in a spool named by the job and all current
   fences. The prototype uses the privileged tree-search broker while the
   production implementation uses an fd-relative userspace walk. A worker
   whose fence was stolen discards its own
   uniquely named spool and cannot overwrite the successor's.
5. Build and validate the checkpoint in the job-local staging database.
   Compute object/ref counts and a canonical state hash while doing the already
   O(namespace size) scan. Claim independent revision/checkpoint leases.
6. In one `BEGIN IMMEDIATE` publication transaction, recheck the operation
   and revision/checkpoint fences, import canonical rows, publish revision `R0`
   and its checkpoint, change the reserved watch to active with indexed
   revision R0 and physical cut S0 at sequence 0, install distinct
   `watch-indexed-head` and `watch-last-cut` pins, remove the operation pin, and
   set `replay_floor_seq=0`, then finish the operation. Sequence 0 is
   represented by the watch heads and has
   no `watch_cuts` row; this same rule lets multiple worktree watches share one
   seed revision/snapshot.
7. Return core cursor `(watch_id, 0)` with `fresh_instance=true`. Initialize
alone does not mint an fsmonitor clock. After the namespace daemon binds the
exact root/grant and successfully arms both the mandatory root-path-binding and
mount-topology monitors, the first ordinary query takes a later cut and
establishes a clock. Only Worktree's separately proved seed protocol may mint a
sequence-0 clock.

Initialization is O(namespace size). It must never index the mutable source and
then assume the scan represents one instant; the RO cut is the consistency
boundary.

### 8.2 Changes(watch)

1. Acquire the watch's cut lease in a short transaction, incrementing its
   fencing token. The lease serializes snapshot creation for this writable
   source, not the later kernel comparison.
2. In that transaction reserve sequence `n` as one greater than every prior cut
   or operation sequence for the watch, together with base cut A, operation ID,
   and the deterministic path in `operations`, and pin A. Sequences are never
   reused after a terminal failure. There is exactly one durable operation for
   `(watch, n)`. If its lease is stolen, the new worker resumes that path/intent
   rather than creating another cut.
3. In one short fenced transaction change the operation from `planned` to
   `fs_started`, durably closing its query-admission batch. Then create RO
   snapshot B of the live source outside SQLite. If the optional precision
   guard is active, drain its private marker after the snapshot and copy the
   certified durable epoch/head sequence into the operation; if that fails,
   leave both columns NULL. In another fenced transaction verify B, insert its
   snapshot row and target operation pin, and
   commit. With no DB write transaction open, validate that immutable B has no
   nested-subvolume boundary; if this cannot be proven, block the watch and do
   not publish an incremental result. In a second fenced transaction insert the
   A -> B `watch_cuts` row, insert B's `watch-last-cut` pin, advance only the
   physical `last_cut` head with a fence-and-sequence CAS, and remove A's old
   `watch-last-cut` pin. The update must affect exactly one row, or match an
   explicitly verified idempotent state already at B/n; every other result
   rolls back. A's separate indexed-head/operation pins remain. Release the cut
   lease. The next caller may now create C while A -> B is still indexing.
4. Conditionally pin A and B only while both are `present`, then claim the
   unique comparison job. The job claim and pins share one `BEGIN IMMEDIATE`
   transaction. Two identical requests share
   `(A, B, comparison_kind, algorithm_version)`; a reclaimed lease increments
   `lease_fence`, so its old worker cannot publish.
5. Run the changed-object ioctl outside SQLite into a `.part` file whose name
   contains the comparison job ID and every relevant fence. On success, parse
   every record, validate stream and external snapshot identities, and hash it.
   Recheck the winning fence before fsync/rename promotion; a stale worker
   discards only its uniquely named file. On ioctl failure, discard all partial
   records. Canonical publication rechecks the fence again.
6. Normalize raw reference sets:

   ```text
   net_add    = raw_add - raw_delete
   net_delete = raw_delete - raw_add
   ```

7. Independently claim or join the build lease for target revision B. Apply the
   normalized object/ref changes to immutable revision A in a job-local
   staging database. For every non-deleted object whose inode item changed,
   obtain target generation/type/mode/nlink/uid/gid/rdev from the v2 record or
   an exact targeted broker lookup. For every xattr change, independently
   obtain the target classified security-xattr digest. This includes chmod,
   ownership, xattr, and link-count changes, not only creates and replacements.
   Inherit an attribute/classification only when its relevant item did not
   change, and update the revision's Worktree safety summary from old/new
   classifications. Validate generations, delete/add preconditions, reference
   endpoints, reachability, and incrementally derived counts. A flat canonical
   state hash is not required on this path. Preserve every streamed inode-item
   change in `comparison_objects`, even when all public semantic attributes and
   net references normalize equal. In particular, a directory inode change is
   a persistent `directory-dirty-witness`; normalization must never erase it as
   "explained" by one known ref add/delete.
8. Resolve deletions against A and additions/current aliases against staged B.
   Build normalized comparison rows and deterministic events in that private
   staging database. Emit an explicit `directory-dirty-witness` event for each
   surviving changed directory so compatibility projection cannot confuse
   endpoint equality with absence of transient namespace activity. No
   canonical child rows are written across transactions.
9. Wait until sequence n-1 is the indexed head. In one `BEGIN IMMEDIATE`, verify
   the operation, comparison, and revision fencing tokens and both immutable
   snapshot identities; import the staged rows; conditionally insert B's
   `watch-indexed-head` pin; publish comparison/revision B; and advance the
   indexed watch head only with:

   ```sql
   UPDATE watches
      SET indexed_revision_id = :rb,
          indexed_seq = :n
    WHERE id = :watch
      AND indexed_revision_id = :ra
      AND indexed_seq = :n - 1;
   ```

   The CAS must affect exactly one row, or the transaction must recognize the
   fully published idempotent state already at B/n. Any other result rolls back;
   it must not mark the cut ready or return events. In the successful
   transaction, mark `watch_cuts` ready and the operation done, remove A's
   `watch-indexed-head` pin, release all operation/comparison pins which this
   job owns, and retain B's independent `watch-last-cut` pin only if B is still
   the physical last cut. A compatibility cut is now eligible for the separate
   mount-monitor/epoch finalization in Section 10.3, which inserts B's
   `fsmonitor_boundaries` row whether or not the operation captured an optional
   precision cursor. Core Changes publication alone does not mint a clock.

10. Commit before returning any event. Return/replay rows from `change_events`,
   never an in-memory pre-commit stream.

Snapshot cuts for B and C are serialized, but kernel comparisons A -> B and
B -> C may run concurrently. C's manifest may finish first; its revision cannot
publish as the ordered watch head until B's revision is ready, so it remains in
its fence-specific staging file. This gives concurrent expensive work without
allowing callers to stomp on the head.

A historical incremental A -> B request uses the same unique comparison job
but does not mutate a watch head. A and B need not be adjacent, but the database
must prove they are ordered cuts of the same watch branch (including a
worktree's seed -> first-cut edge). If revision B already exists, reuse it and
persist only the requested comparison/events; otherwise it may be built from
revision A. For unrelated or divergent branches, coincidentally equal inode
numbers/generations do not prove object continuity. Return a `full_fresh`
comparison which enumerates B as a fresh instance (and optionally a full path
set diff), rather than applying `CHANGED_OBJECTS` or claiming an incremental
history.

#### Gap recovery

If an intermediate cut is terminally failed or lost, the adjacent CAS must not
be weakened silently. Under a new watch fence, choose a later validated RO cut
T at sequence n and the actual indexed head A at sequence m, where `m < n`.
Build a complete checkpoint for T and a `full_fresh` comparison from A to T in
private staging; its event stream enumerates every visible path in T and marks
`fresh_instance=1`.

The publication `BEGIN IMMEDIATE` proves that:

- the watch still has A/m as its indexed head;
- every intervening cut/operation is terminally failed and fenced against later
  publication;
- T is present, immutable, boundary-free, and pinned; and
- the full checkpoint, comparison, and event counts match the staged result.

It then imports the checkpoint/events, inserts T's indexed-head pin, CASes the
watch directly from A/m to T/n, marks the target cut ready and fresh, removes
A's indexed-head and transient pins, advances `replay_floor_seq` to n (or
rotates the clock epoch), and commits. Exactly one CAS row (or an exact
already-published idempotent state) is required. The physical last-cut head and
its pin remain independent if an even later cut already exists. This is the
only operation allowed to skip a sequence, and consumers reset their state from
the fresh enumeration rather than assuming transient events were preserved.
Publication also rotates the external clock epoch and records a fresh
fsmonitor cut boundary at T. It includes a precision cursor only if T's
operation captured a still-complete guard interval; otherwise subsequent
queries use coarse dirty-witness projection.

### 8.3 Worktree(watch, destination)

1. Run Changes through publication, producing a new RO anchor S and ready
   revision R. This makes the clone and its index describe exactly the same
   state.
2. Acquire the per-filesystem topology lease. Validate that the authorized
   destination-parent fd is on S's filesystem, record its containing-subvolume
   UUID and directory inode+generation plus the single basename, and verify the
   destination does not exist. The manager first proves a random reservation
   name absent, then challenges the unprivileged front end to create that
   mode-0600 regular file in the parent with `O_CREAT|O_EXCL|O_NOFOLLOW` and to
   write a 32-byte operation nonce. Reopen it from the parent fd and record its
   name, inode+generation, nonce, and single-link/type checks. Successful
   kernel-mediated creation is the one-shot proof that this peer could create
   in the directory, but it is **not** authority to relocate an arbitrary
   prepopulated subvolume with preserved owners, ACLs, set-ID bits,
   `security.capability`, device nodes, or security labels. Worktree therefore
   also requires an administrator/trusted provisioner to grant the `WORKTREE`
   bit with a destination/metadata-relocation policy. V1 binds that policy to
   an entire destination subvolume UUID/root identity; the broker proves the
   exact root fd, resolves the normalized relative parent itself with
   `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_XDEV)`, and compares
   the expected directory inode, ownership, mode, and security-xattr hash. It
   also uses `statx(STATX_MNT_ID_UNIQUE)` plus `statmount(STATMOUNT_MNT_BASIC)`
   and rejects `MOUNT_ATTR_IDMAP`; the schema's `allow_idmapped` bit is reserved
   for a future policy version and v1 provisioning rejects it for every policy.
   These broker-observed root/parent/mount/LSM-label facts are authenticated by
   the effect hash stored in the root-owned receipt. A directory can move only
   within the authorized root because a cross-subvolume move fails with
   `EXDEV`, so authorization does not depend on detecting a rename after the
   fact. Narrower directory-subtree policies are unsupported without a distinct
   provisioned subtree identity. The untrusted
   `sanitized-private-user-tree` policy requires a private caller-owned parent
   and both source and destination views to be non-idmapped, with an indexed
   source proven to be wholly owned by that caller and to contain no set-ID
   bits, device nodes, `security.capability`, or
   disallowed `trusted.*`/`security.*` metadata; the active LSM must also approve
   the relocation. It does not rely on `nosuid`/`nodev`, because the same Btrfs
   subvolume may be reachable through another mount alias. An explicit
   `admin-trusted-preserve` policy instead accepts the full preserved metadata
   effects. The
   reservation proves current name-creation access and prevents races; it does
   not replace this policy. Reject a
   destination below **any non-deleted**
   watch (`initializing`, `active`, or `blocked`), because publishing the
   worktree there would introduce a boundary that watch cannot track. In one
   transaction create a fenced Worktree operation containing the source
   snapshot/UUID, requested RW flag, caller/grant identity, immutable Worktree
   policy ID/hash, protected staging path, stable destination-parent identity
   and final locator; reserve
   `(filesystem_id, path)` with a `worktrees(state='creating')` row; and
   conditionally pin S under the operation. Release the topology lease only
   after the reservation is visible. Initialize performs the symmetric check
   against every non-deleted reservation, closing both race orderings.
3. Fence the operation from `planned` to `fs_started`, then call
   `BTRFS_IOC_SNAP_CREATE_V2` without `BTRFS_SUBVOL_RDONLY`, using S as the
   source and a protected `staging/worktree-<operation-id>` path as the target.
   It is valid to make a writable snapshot from a read-only snapshot.
4. Read back the writable subvolume UUID and parent UUID, verify them against
   the intent and seed, and durably record the generated UUID. Reacquire the
   topology lease, revalidate the still-authorized destination fd against the
   stored subvolume UUID/inode/generation, reopen and verify the exact
   reservation inode/generation/nonce, confirms the destination remains absent,
   and rechecks all non-deleted watches, the still-active grant, and its
   Worktree policy. If a sequence-0 facade was requested, the namespace daemon
   must already hold the destination namespace's mountinfo monitor and arm the
   mandatory root-path-binding chain top-down through the destination parent,
   watching the still-absent final basename. Under those monitor locks the broker
   calls `renameat2(..., RENAME_NOREPLACE)` with that fd and basename through a
   common mount of the same Btrfs filesystem, then removes the reservation. The
   daemon drains a marker, accepts only the expected create/move event for this
   operation, re-resolves the full chain to the exact generated UUID, and fails
   seed-clock establishment on any other binding or mount event. The broker
   forces and waits for a Btrfs transaction commit before SQLite can say
   `present`. This closes the external check/create race, avoids symlink
   re-resolution, and is power-loss durable. A destination that cannot be
   reached by a same-filesystem rename is rejected rather than using a
   crash-ambiguous direct create. The reservation is an explicit one-shot
   delegation captured at request time; deployments requiring current DAC/LSM
   re-evaluation at rename time must perform publication in an unprivileged
   helper in the caller's security domain instead.
5. In the publication transaction, mark the worktree and operation complete.
   If tracking was requested, create a new active watch and explicit grant,
   point its sequence-0 indexed revision and physical last cut at R/S, install
   its indexed-head and last-cut pins on S, and link it from the worktree. If
   the namespace daemon completed the pre-rename binding protocol with the
   exact root fd, active owner grant, and synchronized path-binding and
   mount-topology monitors, initialize the facade in
   `snapshot_only` mode and insert a `proved_worktree_seed` fsmonitor boundary
   at sequence 0; otherwise leave fsmonitor disabled. Release the operation's
   source pin. If it is not tracked, simply release that pin. This is O(1)
   database metadata; no checkpoint or ref rows are copied. Release the
   topology lease after publication.
6. The Worktree integration may return the sequence-0 seed clock only to a
   caller which installs R as its exact expected-tree baseline. S is the exact
   RO source from which the child was cloned, and its boundary is explicitly
   distinguished from a normal query cut. A mutation after clone creation,
   including one before rename or response delivery, is later than S and must
   leave the first Changes dirty witness; an unclean boot rotates the clock.
   Thus no validation cut or recursive-watch crawl is required for correctness.
   Generic callers which did not install R receive no seed clock and establish
   a baseline with a fresh query. The optional precision guard can arm
   asynchronously; the first interval without two complete guard cursors uses
   coarse projection.

Future worktree changes snapshot the writable clone to S2 and compare S -> S2.
The physical snapshot ancestry and the logical index edge are both recorded,
but only the explicit DB edge decides which revision is applied.

### 8.4 Garbage collection

Pins are owned by watch physical heads, pending cuts, active comparisons,
revocable `retention_leases`, and worktree watches which still need a seed for
their next comparison. Creating a caller retention lease requires `RETAIN` and
inserts its pin atomically; release, expiry under a new fence, or grant
revocation removes that pin atomically. A writable worktree's mere existence
does not authorize the manager to delete it.

For each unpinned managed RO snapshot:

1. In `BEGIN IMMEDIATE`, verify there are no pins or active jobs and change
   `physical_state` from `present` to `deleting`; in the same transaction create
   or fence its unique `snapshot_delete_operations(state='planned')` intent.
   New jobs cannot pin it.
2. Change the delete intent to `fs_started`, then delete the exact
   FSID/UUID/path outside SQLite through the broker. The root-owned receipt
   journal binds delete operation ID, fence, and target identity, so takeover
   cannot issue a conflicting delete while the first ioctl may still run.
3. Force and wait for a Btrfs transaction commit with `BTRFS_IOC_START_SYNC` /
   `BTRFS_IOC_WAIT_SYNC` (or an equivalently checked `syncfs`) so a power loss
   cannot resurrect the namespace deletion after SQLite says it is final.
   Record `fs_deleted` before the wait and `delete_durable` afterward under the
   same fence.
4. Mark the snapshot `deleted` and the delete intent `done` together, retaining
   UUID, lineage, and tombstone. On a retryable
   failure return it to `present` only after reopening and verifying the exact
   UUID, RO flag, and transaction metadata. An absent, mismatched, or ambiguous
   result remains `deleting` or becomes `lost`; it must not become pinnable.
   Startup also treats a `deleted` row whose exact UUID has reappeared at its
   managed path as a durability fault and reconciles it fail-safe.

Physical and logical GC are separate. A physical snapshot may be removed while
its revision/events remain useful. A revision can be removed only when no watch,
worktree, retained comparison, or descendant delta depends on it; checkpoint
descendants first when necessary. Active query revision/comparison pins block
logical reclamation, and a query lease's guard range blocks mutation-event GC.
Event retention defines when old cursors become stale; advancing either replay
floor and deleting the newly unreachable events is one transaction after all
conflicting query leases are gone or fenced as expired.

Pin acquisition is always a conditional insert in the same writer transaction
as job/cut registration:

```sql
INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason)
SELECT id, :owner_kind, :owner_id, :reason
  FROM snapshots
 WHERE id = :snapshot_id AND physical_state = 'present';
```

The caller requires one inserted row. GC's transition to `deleting` is likewise
a conditional update with `physical_state='present'` and `NOT EXISTS` clauses
for pins and active operations. The trigger provides a second guard. Pending
cuts pin their base when the operation is reserved and their target in the same
transaction which registers the new snapshot.

After a full checkpoint is ready, storage compaction may atomically clear that
revision's `storage_base_revision_id`, set its depth to zero, and remove its
now-redundant overrides. This severs the physical dependency while immutable
comparison provenance remains. Before deleting a tracked Worktree row, one
topology transaction retires its child watch: rotate and disable its clock,
revoke grants/triggers, remove the proved-seed boundary, clear watch heads and
their pins, and mark the watch deleted. Only then may it mark the Worktree
deleted and clear `seed_revision_id`. Old sequence-0 tokens are consequently
unauthorized/stale, and no live boundary refers to a tombstone without seed
provenance. Deleting an ordinary watch similarly clears its heads and pins.
Tombstones therefore do not pin logical revisions forever, and the partial
live-watch uniqueness index permits the same still-existing writable subvolume
to be initialized again.

Because `revisions.provenance_comparison_id` is a foreign key, the lightweight
comparison header/tombstone remains while that revision exists. Retention may
delete its bulky object/ref/event payload after it is no longer replayable, but
not the provenance row itself.

### 8.5 Abort and retry

A retryable failure does not create a new intent. A worker takes over the same
operation, increments its fence, uses new fence-specific spool/staging names,
and resumes or reconciles the deterministic filesystem object. `failed` means
terminal; it is never reused by an unfenced worker.

Every terminal abort is one writer transaction which fences the operation,
clears its lease, releases only its own pins, and performs the operation-specific
transition:

- **Initialize:** change the initializing watch to `deleted`, clear its heads,
  revoke its grants, and schedule any created managed snapshot for the durable
  GC path. The partial uniqueness index then permits a new Initialize.
- **Changes:** mark an existing cut failed, or retain the failed reserved
  sequence in the operation if no cut was created; release the cut lease and
  schedule an unneeded target snapshot for GC. Later cuts use a new monotonically
  increasing sequence, and the ordered stream cannot advance past the hole
  except through Gap recovery.
- **Worktree:** if the exact clone remains in protected staging, schedule its
  durable deletion; mark the worktree `deleted`, clear `seed_revision_id`, and
  release the source pin. Remove the reservation sidecar only by its recorded
  name after reopening and matching its inode, generation, type, link count,
  and nonce. If the expected UUID may already be at the caller path, require
  the authorized parent fd and either finish publication or leave it for
  explicit operator/caller resolution; never delete or replace an ambiguous
  destination or reservation.

Topology leases and watch cut leases are released in the same fenced abort or
left to expire for takeover. Startup performs these transitions for abandoned
operations before admitting a conflicting Initialize, cut, Worktree, or GC.

## 9. Crash recovery and idempotency

Filesystem mutations and SQLite commits cannot be atomic. Every mutating API
accepts or creates an idempotency key and records an operation intent before
the Btrfs action:

```text
Initialize/Cut:
  planned -> fs_started -> fs_created -> uuid_recorded -> manifest_ready
          -> index_committed -> done

Worktree:
  normal:   planned -> fs_started -> fs_created -> uuid_recorded -> done
  restart:  uuid_recorded -> awaiting_destination -> done

Snapshot GC (separate delete intent):
  planned -> fs_started -> fs_deleted -> delete_durable -> done
```

On startup:

- before taking over any operation fence, admitting a response, or completing a
  pending revocation, obtain the broker's old-session fence/drain barrier, then
  query the root-owned receipt journal for every
  manager dispatch intent. Absence of a matching receipt proves the broker did
  not durably start it; each `running`/`needs-reconcile` receipt must reach an
  exact completed, failed-before-effect, or operator-blocked outcome. Reopen and verify the precise target identity for
  snapshot create/delete and external Worktree rename. No conflicting retry,
  fence transfer, publication, deletion, or caller response is allowed while a
  receipt is nonterminal or its outcome is ambiguous;
- intent exists, deterministic path absent: retry or fail the operation;
- protected managed path exists while its operation is not finished: inspect
  it; if a target UUID was already recorded, require it, and otherwise adopt
  only when the intent's source/parent UUID, FSID, flags, path, and immutable
  requested operation match, then record the kernel-generated UUID. The object
  need not encode the creator's expired fence; the takeover's current fence is
  required for publication. Quarantine/report every identity mismatch;
- protected worktree staging path exists: inspect/adopt it under the same rule
  as a managed snapshot and record its UUID. After a daemon restart the
  destination fd is gone, so change the operation to `awaiting_destination`
  until the same authorized principal under the operation's still-active
  original grant resupplies a parent fd. Recheck its FSID,
  containing-subvolume UUID, directory inode+generation, basename, and the
  exact reservation name/inode/generation/type/link-count/nonce. A replacement
  grant cannot resume an operation whose authorization generation was revoked;
  fence and abort it or require operator resolution. `final_path` is diagnostic
  and is never path-walked as recovery authority;
- worktree destination exists after the authorized parent fd is resupplied:
  open the single basename without symlink traversal and adopt only the exact
  target UUID durably recorded before rename. Force/wait for the filesystem
  commit if needed, then finish publication. The exact target UUID at the
  destination proves rename may already have completed even if the sidecar was
  removed; never overwrite an unrelated path;
- `.part` spool exists: delete it unless the fenced worker still owns it;
- complete manifest exists: revalidate and resume indexing;
- building revision exists: resume under a new fence or delete the stale
  job-local staging file;
- client disconnected after commit: replay persisted events;
- snapshot says deleting: reconcile its unique delete intent and broker
  receipt; if the exact UUID still exists, resume under the winning fence, and
  if absent, force a transaction commit before advancing the intent to durable
  and finalizing the tombstone; and
- snapshot says deleted but its exact UUID reappears at the managed path after
  recovery: report a durability fault and return it to the fenced deletion
  workflow rather than treating the path as unrelated or silently overwriting
  it; and
- a boot-ID change, unclean manager handoff, namespace-daemon restart, lost
  mountinfo monitor fd, lost/overflowed mandatory path-binding fd,
  remount/rollback ambiguity, or source identity/transaction discontinuity
  rotates every affected clock epoch before serving it. A clean same-boot
  restart of a separable precision producer, overflow of its distinct recursive
  inotify fd, or loss of one of its subtree watches only marks the optional
  guard gapped and falls back to coarse dirty witnesses, provided both original
  mandatory monitors remain live.
  Recovery abandons waiting cut admissions according to their owning
  operation/connection state and reclaims expired query leases/pins and trigger
  runs under new fences.

The startup implementation recognizes only exact `manifest-...part`,
`full-index-...`, and `target-index-...` private spool formats and verifies
owner, mode, link count, and regular-file type before unlinking. After intended
operations have been reconciled, exact manager-looking `cut-*`,
`cut-initialize-*`, and `worktree-*` names which are not claimed by a durable
snapshot or nonterminal operation are renamed into the manager's mode-0700
`quarantine/` directory. They are never adopted or deleted. A path recorded as
`deleted` or `lost` which reappears is a durability fault and blocks startup.

Fencing tokens, not lease time alone, prevent a paused worker from publishing
after recovery assigned its operation to another worker.

## 10. jj-vcs and Git filesystem-monitor compatibility

### 10.1 Compatibility boundary

V1 targets the behavior actually used by:

- jj revision `6258a85e4b62cea5c81dc5d0687ea4597762c069` with
  `watchman_client` 0.9.0; and
- Git revision `13c7afec212fc97ce257d15601659314c6673d6c`
  (`v2.55.0-424-g13c7afec21`) and its fsmonitor hook-v2 contract.

Those source revisions define the compatibility contract. The current UML
acceptance image also exercises the implementation end to end with the
unmodified binaries available in that image: jj
`0.43.0-28e25c32bc98b6cfba430b4fa44f86141e94266a` and Git `2.43.0`.
Passing those older binaries is useful interoperability evidence, but does not
replace fixtures captured from the pinned contract revisions.

Upgrading either client requires rerunning recorded request/response fixtures.
This is deliberately a compatibility facade over snapshot cuts, not a claim to
implement Watchman subscriptions, SCM-aware clocks, saved state, globbing,
content hashes, unilateral messages, or a semantic filesystem-operation audit
log. Correctness comes from transaction-bracketed persistent dirty witnesses
plus mandatory root-path-binding and mount-topology continuity; an optional
namespace journal adds precise transient names and low-latency trigger wakeups.

There are three front ends over one internal query API:

| Consumer | Front end | V1 status |
| --- | --- | --- |
| jj | Focused Watchman BSER-v2 Unix socket | Required |
| Git | Direct `core.fsmonitor` hook-v2 executable | Required and preferred |
| Git | Bundled hardened Watchman-style JSON adapter plus focused CLI shim | Planned optional compatibility path for UTF-8 roots and names; not required for v1 |
| Git's unmodified stock `fsmonitor-watchman` Perl hook | Focused JSON CLI shim | Future conformance target only under a restricted safe root/name policy; not a supported security boundary |

Git v1 therefore does **not** depend on the Watchman protocol. Only jj-vcs uses
the focused BSER endpoint. Git consumes the same cuts, clocks, revisions, and
projections through the direct hook-v2 adapter. This keeps the shared
correctness model small without emulating Watchman's general command language.

The complete client-visible surface required for v1 is:

| Caller phase | Accepted request | Response consumed by the client | Primitive implementation |
| --- | --- | --- | --- |
| jj socket discovery | `watchman --output-encoding bser-v2 get-sockname` | `version`, namespace-specific `sockname` | Select/start the per-UID namespace daemon; no cut |
| jj repository registration | `watch-project(ROOT)` | exact `watch`, `watcher`, no `relative_path` | Revalidate an exact-root watch or run **Initialize** |
| jj working-copy snapshot | `query(ROOT, {since?, expression, fields:["name"], sync_timeout?})` | `clock`, `is_fresh_instance`, bare relative names | Run/join **Changes**, pin `(A,B]`, project exact names or fresh `/` |
| jj explicit baseline/debug path | `clock(ROOT, {sync_timeout:60000})` | `clock` | Run/join **Changes**; safe use is restricted by Section 10.8 |
| jj trigger disabled | `trigger-del(ROOT, "jj-background-monitor")` | idempotent deletion result | Delete the caller's durable trigger row; no cut |
| jj trigger enabled | `trigger-list`, then the one fixed `trigger` definition | fixed trigger metadata | Persist one fenced trigger; periodic/precision wakeups run **Changes** |
| Git index refresh | `git-fsmonitor-hook 2 OLD_TOKEN` | `NEW_TOKEN\0PATH\0...` | Run/join **Changes**, project Git prefixes or `/`; no Watchman command |

Everything else is outside the contract. In particular v1 has no
subscriptions, `watch-list`, `watch-del`, `watch-del-all`, glob or suffix
generators, `relative_root`, SCM-aware clocks, saved state, content hashes,
unilateral messages, arbitrary trigger programs, or JSON query CLI. Unknown
commands and unsupported query keys are hard errors, never ignored options.

All front ends call one byte-oriented internal operation; they do not maintain
independent watcher state:

```text
FsmonitorQuery {
    watch_id,
    authorization_id,
    old_clock?,
    consumer: JjExactPaths | GitPrefixes,
    filter: ExcludeGitAndJj | ExcludeGit,
}

FsmonitorResult {
    target_clock,
    fresh,
    exact_paths[],
    recursive_prefixes[],
}
```

`exact_paths` and `recursive_prefixes` are raw relative path bytes. The jj
adapter may return only exact paths, so any relevant recursive prefix which
cannot be expanded within its budget changes its result to `fresh`. The direct
Git adapter can encode a recursive prefix with a trailing slash and can encode
a whole-tree invalidation as `/`. The common query engine filters and
deduplicates first, then the required wire adapter performs BSER or NUL framing;
no adapter is allowed to reinterpret a partial result as complete.

The compatibility calls map to the four core primitives as follows:

| Client operation | Core operation and durable effect |
| --- | --- |
| Discovery / connect | Select or start the per-UID, per-mount-namespace daemon. No snapshot or index mutation. |
| `watch-project(ROOT)` | Resolve the exact subvolume identity and grant. If absent, run **Initialize** and publish sequence 0; if present, revalidate the root binding. Never choose an ancestor watch. |
| `query(ROOT, since=A)` and the native Git hook | Run or join **Changes** to create target cut B, then pin and union every adjacent comparison in `(A, B]`. Project the union into exact paths, Git prefixes, or a fresh result and return B's clock. |
| `query(ROOT)` without `since` or with an unusable token | Run or join **Changes** for B and return a fresh/full-invalidation result carrying B's clock. This establishes a future incremental baseline; it does not enumerate a supposedly complete current tree. |
| `clock(ROOT)` | Run or join **Changes** for B and return B's clock without projecting paths. This is safe only for the baseline cases in Section 10.8. |
| Fixed jj trigger registration/list/deletion | Mutate only the durable trigger scheduler rows. Registration queues one run; periodic or precision-guard wakeups cause the scheduler to run/join **Changes** and evaluate the same internal query projection. |
| **Worktree** result | Create a distinct child watch which shares the seed revision. When the caller installs that exact revision as its expected tree, return its authenticated `proved_worktree_seed` sequence-0 clock; otherwise require a fresh query. |
| History removed by **Garbage collection** | Advance the replay floor atomically with deletion. An older client token becomes fresh; it is never answered from a net endpoint comparison which omits intervening witnesses. |

Thus a compatibility query always has a newly committed immutable target cut
(possibly shared with concurrently admitted callers). It never answers from a
mutable in-memory event cache or from "the most recent cut" merely because the
cut is recent. Snapshot creation is the synchronization barrier; the indexed
change stream is the durable replay record; SQLite clocks, boundaries, floors,
and leases make that record safe under concurrency and GC.

The installed entry points are deliberately small:

- `btrfs-awacs-watchman` is the per-user namespace daemon and BSER endpoint;
- `watchman` implements only BSER-v2 `get-sockname` discovery in v1. It
  activates that daemon but never becomes a second watcher; and
- `git-fsmonitor-hook` speaks hook protocol v2 and calls the daemon's native
  query method directly, avoiding JSON and the generic Watchman command path.

The daemon translates client frames and owns the mandatory namespace-view
monitors. The unprivileged manager owns cut coordination, SQLite, index lookup,
projection, clocks, and GC. The privileged broker is used only for the fixed
Btrfs operations described in Sections 5 and 6; it never parses BSER,
stores client clocks, evaluates expressions, or runs triggers.

V1 registers only an exact canonical managed subvolume root. `watch-project`
returns that same path as `watch`, with no `relative_path`; it never coalesces a
project under an ancestor watch. This matches the service's subvolume/worktree
model and avoids a jj bug-shaped edge: jj propagates `relative_path` into normal
queries but not into trigger `relative_root` or `chdir`. A future logical
subdirectory watch must add an explicit raw-byte prefix to the schema and use
it consistently for queries, clocks, trigger filtering, and trigger cwd.

V1 permits one fsmonitor owner grant and one mount namespace per watch. Other
grants may use the core snapshot/index APIs, but cannot obtain clocks or
register triggers for that watch. The owner also has one pinned process-root
identity; a different chroot is a different view even in the same mount
namespace. Supporting multiple fsmonitor principals or views requires a
separate root locator, clock epoch, precision guard, and trigger namespace per
`(watch, grant, mount namespace, process root)` rather than sharing the fields
on `watches`.

#### 10.1.1 Client setup and call sequences

The package installs one binary under three names. The `watchman` name is
discovery-only, `btrfs-awacs-watchman` is the namespace daemon, and
`git-fsmonitor-hook` is the direct Git adapter. The manager database, managed
snapshot directory, private spool, broker socket, and automatic-registration
root are deployment configuration; clients never supply those authorities in
their requests.

For jj, enable exactly the supported backend:

```toml
fsmonitor.backend = "watchman"
fsmonitor.watchman.register-snapshot-trigger = false
```

The connector discovers the socket, sends `watch-project`, and on every jj
operation sends `trigger-del` when the trigger setting is false. When it is
true, it sends `trigger-list` and conditionally the one fixed `trigger`
definition. Working-copy snapshotting then sends one name-only `query`; the
saved opaque string clock from a successful prior snapshot becomes `since`.
No saved clock means a fresh query and full crawl. A query failure also causes
jj to crawl and prevents it from advancing to an unproved service clock.

For Git, configure the direct executable and pin hook protocol v2:

```sh
git config core.fsmonitor /usr/libexec/btrfs-awacs/git-fsmonitor-hook
git config core.fsmonitorHookVersion 2
```

Git invokes the helper in the exact worktree root with `2 OLD_TOKEN`. The
helper uses the same namespace discovery path, sends one focused Git-flavor
query, and converts its result to hook-v2 NUL framing. It never launches the
generic `watchman` CLI for a query. Each linked worktree is registered as a
different exact-root watch; copying a token between worktrees yields the `/`
full-invalidation result.

Automatic registration is a daemon policy, not a Watchman protocol feature.
Activation preconfigures one exact seed root whose mandatory binding proves the
daemon's process-root and mount view. An unknown `watch-project` is admitted
only after the transport proves that the caller has the seed registration's
UID/GID and exact process-root/mount view. The daemon then canonicalizes the
requested path in that shared view, reuses an active exact-path UID grant when
one exists (including a tracked Worktree watch), or runs **Initialize** under
the same UID policy, arms that root's own mandatory monitor, and inserts it in
the concurrent registration table. It never resolves an unknown path merely
because it shares a textual ancestor with an existing watch.

#### 10.1.2 Normative compatibility subset

The following is the complete v1 compatibility contract. Every row is built
from a synchronized **Changes** cut and the retained comparison stream; none is
implemented by independently crawling the mutable worktree inside the daemon.

| Surface | Required by | Implementation on the snapshot/index primitives |
| --- | --- | --- |
| `watchman --output-encoding bser-v2 get-sockname` | jj socket discovery | Select the caller's mount-namespace daemon, activate it under a per-namespace lock, and return its mode-0600 socket. This is a discovery shim, not another watcher. |
| `watch-project(ROOT)` | jj registration | Resolve one exact subvolume root in the authenticated view. Reuse its active watch, run **Initialize** for an authorized arbitrary root, or attach a tracked Worktree to its shared seed revision. Return no `relative_path`. |
| `query(ROOT, {since, expression, fields:["name"]})` | jj working-copy snapshot and the internal Git adapter | Coalesce a new RO cut B, transactionally pin every retained comparison from the authenticated old clock A through B, project all changed aliases/names, apply the fixed client expression, and return B's clock. Missing continuity returns the fresh `/` sentinel. |
| `clock(ROOT, {sync_timeout})` | jj baseline/debug APIs | Take and publish the same synchronized cut as `query`, but return only its clock. It is safe only for the proved-baseline cases in Section 10.8; it is not a crawl-then-clock primitive. |
| `trigger-list`, fixed `trigger`, fixed `trigger-del` | jj's optional snapshot trigger | Store only `jj-background-monitor`. Periodic Changes cuts are authoritative; a complete matching range schedules one fenced, non-overlapping `jj --quiet util snapshot`, and mutations during a run schedule one rerun. |
| Hook-v2 `git-fsmonitor-hook 2 OLD_TOKEN` | Git index and untracked-cache refresh | Query the same facade with Git's `.git` exclusion, then encode `NEW_TOKEN NUL PATH NUL ...`. Empty/numeric/foreign/expired tokens return `NEW_TOKEN NUL / NUL`; failures exit nonzero. |

The daemon intentionally does **not** implement subscriptions, unilateral
Watchman recrawls, cookies, SCM clocks, glob/suffix/type/exists expressions,
content hashes, file-stat fields, project coalescing, arbitrary triggers, JSON
query mode, or Git hook protocol v1. An unsupported command or option is an
error. This closed subset is what lets clocks remain exact capabilities for
immutable indexed cuts instead of inheriting general Watchman state semantics.

For a normal existing directory, the first `watch-project` necessarily pays
the O(namespace) **Initialize** cost. A service-created Worktree instead shares
its seed revision without copying index rows, so registration and snapshot
creation are O(1) in namespace size; only later changed-object application and
the names actually returned are proportional to change volume. A directory
rename can still require O(subtree size) output for jj because its matcher
needs leaf names, as specified in Section 10.4.

### 10.2 Focused Watchman wire and commands

The per-user daemon accepts BSER-v2 framed values on
`$XDG_RUNTIME_DIR/btrfs-awacs/mnt-<namespace-dev>-<namespace-ino>/watchman.sock`.
Each frame begins with the BSER-v2 and zero-capability bytes
`00 02 00 00 00 00`, followed by a BSER integer payload length and the payload.
It applies PDU length, nesting, array, and string limits before allocating
result-sized structures.
It accepts both BSER byte strings (`0x02`) and BSER-v2 UTF-8 strings (`0x0d`),
validates the latter, emits `0x0d` for valid text, and retains `0x02` for
arbitrary non-UTF-8 Unix path bytes. Object keys follow the same rule. This is
part of the required jj interoperability contract, not an optional extension.
Facade activation also requires kernel-supplied `SCM_PIDFD` support; v1 has no
numeric-PID fallback because PID reuse would make process-root and mount-view
checks racy.

When `WATCHMAN_SOCK` is unset, `watchman_client` discovers the socket by running:

```text
watchman --output-encoding bser-v2 get-sockname
```

The installed `watchman` shim returns a BSER object containing `version` and
the namespace-specific `sockname`. It first activates the per-user daemon
(preferably through a namespace-aware user socket unit; otherwise under that
namespace's `daemon.lock`) and verifies that the resulting socket has the
expected type and owner. Setting `WATCHMAN_SOCK` bypasses the shim and connects
jj directly, so that deployment must arrange activation before jj starts; the
daemon still validates the connecting process's mount namespace.

The prototype activation fallback is environment-configured: root, managed
snapshot directory, private spool, manager database, and broker socket are
supplied through `BTRFS_AWACS_*`. Explicit watch/grant IDs select an existing
registration; when both are omitted, the daemon reuses the active canonical
root/UID grant or transactionally creates the manager store and runs
**Initialize** before binding the socket. The namespace lock prevents two
discovery calls from racing that choice. A future distribution may replace
these variables with a credentialed registration service without changing the
socket or clock contracts.

The socket implements only these semantic command arrays:

```text
["watch-project", CANONICAL_ROOT]
["clock", WATCH_ROOT, {"sync_timeout": 60000}]
["query", WATCH_ROOT, QUERY]
["trigger-list", WATCH_ROOT]
["trigger", WATCH_ROOT, TRIGGER]
["trigger-del", WATCH_ROOT, "jj-background-monitor"]
```

`watch-project` resolves an active watch or synchronously runs Initialize when
policy authorizes automatic registration. Its response contains `version`, the
exact `watch` root, `watcher: "btrfs-index"`, and no relative path. Querying or
clocking an unregistered root is an error; a path string never implicitly
selects a different managed root.

`clock` performs the same synchronized cut and mandatory view checks as a query but
returns only `{ "version": VERSION, "clock": NEW_CLOCK }`. It does not mean
that an earlier independent crawl describes that cut; Section 10.8 defines the
only safe baseline uses.

`query` accepts only:

- an omitted `since` or a plain string/integer clock;
- `fields: ["name"]` exactly;
- the default synchronized-query timeout behavior;
- no `relative_root` in v1; and
- the expression subset `true`, `false`, `not`, `allof`, `anyof`,
  `name(..., "wholename")`, and `dirname` needed by the two clients.

Unsupported fields, generators, clock forms, expression terms, or options are
errors, never silently ignored or approximated. In particular, SCM-aware
clocks and `empty_on_fresh_instance` are not accepted. Results contain bare
relative byte-string names, not `{name: ...}` objects:

```text
{
  "version": VERSION,
  "clock": NEW_CLOCK,
  "is_fresh_instance": BOOLEAN,
  "files": ["path", ...]
}
```

The v1 `watchman` executable deliberately has no JSON command mode. A future
optional hardened adapter may add three JSON CLI behaviors: one `query` on
`watchman -j --no-pretty`, `watch ROOT`, and `clock ROOT`. That addition must
return the Watchman-style "directory is not watched" error for unknown-root
queries, perform authorized Initialize only for `watch`, begin every JSON
response with `{` at byte zero, keep diagnostics on stderr, serialize with a
real encoder, and invoke subprocesses without a shell. It must not change the
BSER daemon or direct Git hook contracts.

The unmodified Git sample interpolates the worktree root into both raw JSON and
shell command strings, and interpolates a `c...` token into JSON without
escaping. Quotes and backslashes can corrupt the request, while shell syntax in
a legal pathname can be executed during its unregistered-root fallback. It is
therefore not a supported front end for arbitrary roots. A deployment using it
must pre-register roots and enforce a documented restricted alphabet for both
roots and UTF-8 names; the v1 service's security and correctness contract uses
the direct helper instead. A future bundled hardened adapter must meet the same
boundary.

The internal index, mutation journal, and direct Git hook preserve arbitrary
non-NUL Unix path bytes. The pinned `watchman_client` 0.9.0 `NameOnly` path
deserializes through `PathBuf` and rejects non-UTF-8 BSER strings, so current jj
does not. If a jj query result contains such a path, the focused endpoint
returns an error. The pinned jj then attempts its full-crawl fallback, but that
crawler also rejects the non-UTF-8 `DirEntry`; the snapshot fails before a
cleared clock is persisted. Such repositories are unsupported by the jj facade
until both `watchman_client`/`NameOnly` and jj's crawler expose raw byte paths.
Any future JSON front end will likewise support UTF-8 only. Embedded NUL is impossible in a
filesystem component; `/`, empty components, leading slash, `.` and `..` are
rejected from normal internal paths.

### 10.3 Clocks are authenticated snapshot cuts

An external clock is an opaque ASCII string beginning with `c:`. Its entire
alphabet is normatively `[A-Za-z0-9:._-]+`; quotes, backslashes, whitespace,
control bytes, and padding are forbidden. V1 encodes it as
`c:btrfs-awacs:1:<base64url-no-padding(payload || HMAC)>`. This is safe even for the
stock adapter's unescaped `since` interpolation. Its authenticated payload
binds at least:

```text
clock-format-version
store UUID
watch ID
watch clock epoch
cut sequence
fsmonitor owner grant ID
view-monitor session ID
boundary kind (`cut` or proved Worktree seed)
optional precision-guard epoch and sequence
comparison algorithm version
target snapshot UUID
```

The binary payload layout remains private and versioned. Decoding a clock never
authorizes a watch, and no caller-supplied sequence or Btrfs root ID reaches the
broker. A structurally invalid or merely foreign old token is a request for a
fresh baseline, not broker input.

Before minting the first clock, the namespace daemon activates the facade. It
canonicalizes and opens the exact root with `openat2` relative to the
authenticated peer's process-root fd using
`RESOLVE_IN_ROOT|RESOLVE_NO_MAGICLINKS|RESOLVE_NO_SYMLINKS`. V1 rejects both
ordinary symlink and procfs-style magic-link components rather than pretending
their retargeting is covered by a directory watch. On a dedicated mandatory inotify fd it
watches each parent before resolving the next raw component, records that
component's inode/mount identity, drains a private marker, then re-resolves and
verifies the entire chain and live subvolume UUID. `IN_CREATE`, `IN_DELETE`,
`IN_MOVED_FROM`, `IN_MOVED_TO`, or relevant `IN_ATTRIB` for the next component,
self move/delete,
`IN_IGNORED`, queue overflow, unmount, an unknown event, or failed re-resolution
rotates that watch's clock epoch. This catches a root or ancestor which is
renamed/replaced and restored between cuts; simply reopening the final path
would not. If any ancestor cannot be watched, the facade is unavailable.
The same is true if a component filesystem does not provide trusted inotify
coherence; the core snapshot API remains available.

The manager rejects current descendant mounts and records the active grant,
raw client-view path, mount namespace, and process-root device/inode/mount ID.
It also records the daemon's random in-memory monitor-session ID; a reconnect
with a different ID rotates the clock even in the same boot. The daemon keeps
that namespace's `/proc/self/mountinfo` open. One
serialized monitor owner polls the fd; whenever
`POLLERR|POLLPRI` is observed it rotates the clock epoch of **every** watch
bound to that monitor durably before unlocking or allowing boundary
finalization. Plain mountinfo polling reports that the namespace changed, not
which subtree changed; reparsing afterward can reject mounts which still
exist, but cannot scope a transient attach+detach. Loss of the fd or owner is
the same all-watches continuity gap. A short writer transaction claims the
watch's single owner slot and sets `fsmonitor_state='snapshot_only'`; the first
query can now mint a coarse but correct clock. A future mount-event API with
stable affected mount IDs may safely narrow the rotation.

The optional precision guard arms separately. The daemon installs recursive
inotify coverage top-down. Before certifying **every** precision boundary, not
just initial arming, it emits a marker in its private runtime directory watched
by the same inotify fd and waits until all events through that marker are
durable. New/moved-in directory coverage remains imprecise until its watches
and another marker complete. Failure, overflow, or watch loss changes the state
to `guard_gapped`; the cut still gets a snapshot-only boundary. No other grant
may steal either owner slot without terminal revocation and epoch rotation.

Both `clock` and `query` are synchronization barriers:

1. Authenticate the frame's pidfd/credentials and grant, resolve the exact
   watch, and admit the request to that live source's cut coordinator. Under
   the binding and mount-monitor locks, drain a binding marker, poll the
   already-open mountinfo fd, durably rotate on any relevant event, re-resolve
   the full raw component chain, verify its exact inode/mount/subvolume UUID,
   reparse to reject a currently mounted descendant, and only then admit the
   request. The freshly resolved root fd is the source of the cut; a stale fd
   which still opens an unlinked/moved subvolume is never substituted.
2. In one short writer transaction close the admitted batch with
   `planned -> fs_started`, and commit before issuing the snapshot ioctl.
   Requests queued before one cut starts may share it; requests arriving after
   batch close use a later cut. A pre-request indexed head is never returned
   merely because it is recent.
3. Create the RO cut, then—if the precision guard is active—drain its private
   marker and copy the resulting durable `(guard_epoch, guard_sequence)` onto
   the operation. Events between the snapshot and marker are harmlessly
   reported early in this interval; events after the marker belong to the next
   one. If the marker cannot be certified, omit the cursor and use coarse
   projection.
4. Run the normal Changes protocol through durable index/event publication.
   Physical snapshot creation is serialized per live source, while comparison,
   path derivation, and SQLite staging for immutable cuts may run concurrently.
   After the cut is ready, boundary finalization again drains the mandatory
   binding marker, polls the mountinfo monitor, re-resolves the exact root
   chain, verifies no descendant mount and the same clock epoch, then inserts
   B's fsmonitor boundary. It includes the optional cursor only if both its
   separate precision marker and guard epoch remain complete.
5. Return a clock only after its target revision, cut, and boundary are
   committed. Immediately before writing bytes, renew and validate the query
   fence under the grant response gate and repeat the binding-marker,
   root-resolution, mount-monitor, monitor-session, and epoch checks, including reopening the
   frame sender's current `/proc/<pid>/root` and mount namespace through the
   same live pidfd identity. An intervening binding/mount event rotates the
   epoch; a changed/exited sender rejects the request. Either case discards this
   response and retries, when appropriate, from a fresh boundary. A response
   never combines B's clock with paths
   derived only through A or with an uncommitted comparison.

Disconnecting a waiter does not cancel a shared cut. The request timeout may
stop waiting and return an error, but the fenced operation can finish for other
waiters and triggers. No Btrfs ioctl, path expansion, or client write occurs
inside a long SQLite write transaction.

For `query(since=A)` targeting B, the service walks every ready adjacent cut in
`(A.cut_sequence, B.cut_sequence]`, not merely a new net A -> B comparison. It
always unions their conservative dirty-witness projections. If A and B also
carry retained complete cursors in the same precision epoch, it reads and
unions every mutation event in `(A.guard_sequence, B.guard_sequence]`; only
that case permits exact namespace events to replace coarse changed-directory
fallbacks. A path observed at an intermediate cut or by the precision journal
therefore remains dirty even if a later cut changes it again. If there were no
intermediate cuts, the ordinary A -> B comparison is the immutable-state
portion of the range. The result is byte-sorted for reproducibility; consumers
must not attach meaning to order.

External clocks are soft state and do not pin history forever. GC maintains a
count/time retention window and advances `replay_floor_seq` atomically with
removing the history needed before that floor. A clock below the cut floor
becomes fresh. Guard history has its own floor, advanced only after protected
query ranges are gone; a cursor below that floor merely loses precision and
uses coarse projection. Clean same-boot restarts preserve clocks when
snapshot/source continuity is proven and the original binding/mount monitors
remain live. Namespace-daemon restart, boot change, unclean manager handoff,
binding/mount-monitor loss or events, remount/rollback, or source ambiguity
rotates clock epochs. Restart of only a separable optional precision producer
rotates its guard epoch.

### 10.4 Projecting object changes to dirty paths

The adapters expose a conservative re-stat set, not the semantic ChangeSet.
False positives are acceptable; a false negative can make jj or Git report a
falsely clean working copy.

| Indexed evidence | Exact names always available | Snapshot-only fallback |
| --- | --- | --- |
| create/delete/ref add/ref remove | Every target/source name represented by endpoint refs | Also apply the changed-parent directory witnesses below |
| file data, xattr, mode, type, or other object change | Every target alias, plus any removed/renamed source alias in the range | No additional namespace fallback for a non-directory object |
| hardlinked object change | Every surviving target alias; explicit ref changes add old/new names | Also apply every changed-parent directory witness |
| file/symlink rename | Both old and new names | Also apply both changed parents' directory witnesses |
| directory subtree move | Old and new prefixes plus the client-specific handling below | Coarse-invalidate every old/current changed-directory prefix |
| surviving changed directory inode | Its old/current directory aliases | Coarse-invalidate the entire old/current subtree; root means the whole tree |

For each adjacent comparison, old names are resolved against its source
revision and new/current names against its target revision. This is why the
inode-to-reference index, rather than one arbitrary parent, is required for
hardlinks. Names from excluded directories remain in the canonical index and
history; expressions filter only the final presentation set.

The directory rule is normative. If an interval lacks two retained, complete
precision cursors in the same guard epoch, **every** surviving directory whose
inode item changed is a coarse subtree witness, even when the interval also
contains understandable net ref additions or removals in that directory. Net
endpoint refs cannot prove that no additional name was created and removed
between cuts. For example, creating and deleting `d/transient` plus retaining
`d/persistent` changes the same parent inode; accounting for the persistent ref
must not erase the witness for the transient name. The root witness maps to
Git's `/` sentinel and to a fresh result for jj. A non-root witness maps to old
and current trailing-slash prefixes for the direct Git hook; because jj treats
returned names as exact files rather than recursive prefixes, its common query
becomes fresh unless its expression proves the entire witnessed subtree is
excluded.

This fallback is correct but deliberately coarse. In snapshot-only mode,
almost every create, delete, link, unlink, or rename changes a parent directory
and therefore makes jj crawl again; data-only edits remain alias-precise. A
complete namespace journal is optional for correctness but practically
required for Watchman-like incremental performance on namespace-heavy
workloads.

When both boundaries have a retained complete precision interval, its
path-level create, delete, move, and attribute events replace—not merely try to
"explain"—the coarse directory-witness fallback for that interval. Indexed
object/ref evidence is still unioned in, so writable `mmap`, hardlink aliases,
or another event class which inotify does not describe cannot disappear. A
content/object journal notification uses its observed inode+generation and the
source/target indexes to expand every hardlink alias. If the producer cannot
prove coverage or alias identity, the interval uses the coarse witness instead
of mixing a partial journal with an exact claim.

Recursive inotify is armed top-down with parent watches installed before
enumerating children, then drained through a marker in the daemon's private
runtime directory watched by the same inotify fd before the guard becomes
active and after every cut. Creating or moving in a directory first records a
`directory-prefix` event for that known prefix, because activity may occur
before recursive coverage completes; after its watches and another marker are
certified, later events can be exact. For the direct Git hook the prefix is a
trailing-slash invalidation. For jj it causes fresh unless the fixed expression
excludes the whole prefix. Queue overflow, `IN_IGNORED`, unmount, permission
loss, an unexpected watch disappearance, or an event whose affected prefix is
unknown gaps the guard and restores snapshot-only projection; it never turns a
partial event set into an empty success.

jj sends this exact expression and asks only for names:

```json
["not", ["anyof",
  ["name", [".git", ".jj"], "wholename"],
  ["dirname", ".git"],
  ["dirname", ".jj"]
]]
```

Git's stock adapter uses:

```json
["not", ["dirname", ".git"]]
```

Deleted names are evaluated lexically; the service does not require the path
to exist in B. There is no ignore-file or `exists` filter. jj applies its own
sparse and ignore matchers; a changed `.gitignore` is returned and jj expands
its parent subtree itself.

A compact `subtree-moved` event needs special treatment:

- Git's direct hook emits both old and new directory prefixes with trailing
  `/`; Git invalidates the indexed cone and corresponding untracked cache.
- The common Watchman query emits those prefixes and also expands old and new
  descendant leaf names from the two immutable indexes. The prefixes cover
  Git's sparse-index/directory invalidation; jj's exact-file matcher does not
  recurse merely because it receives a directory name, so its correctness
  comes from the expanded leaves.
- If expansion exceeds the configured path, byte, or time budget, the result
  is fresh rather than partial. The cut remains valid and is returned as the
  new baseline.

This expansion is the honest non-O(1) corner. Snapshot creation remains O(1)
in namespace size, but a client demanding one dirty name per moved descendant
has O(subtree size) output.

### 10.5 Fresh instances and failures

The internal query result is fresh when `since` is omitted, malformed,
unauthenticated, foreign to the watch/store/epoch, below the replay floor,
bound to a missing logical cut/revision or reclaimed event chain, incompatible
with the algorithm, or separated from the target by a failed/lost/full-fresh
cut. A physically deleted snapshot is not by itself fresh when its immutable
revision and every required event remain replayable. Corruption, recovery
ambiguity, or a path-expansion limit has the same result. The daemon still
completes a new cut B first, so the returned clock is a valid future baseline.
A coarse directory witness which the common Watchman response cannot represent
recursively, a scoped/unscoped precision invalidation, or a result-size budget
also makes jj's result fresh. The direct Git adapter expresses the same
complete or prefix invalidation as `/` or `dir/`. A changed root inode is fresh
only when the interval lacks a complete precision journal; exact journaled
root-level names may replace that coarse fallback.

A successful fresh result requires a committed B boundary, the verified
persistent dirty-witness ABI, and continuous source, root-path-binding, mount,
boot, and authorization epochs. It does **not** require the optional precision
guard. The client crawls after B; every later in-root mutation persists into a
B -> next-cut object or directory witness, while an external path/mount view
change rotates the clock. If the dirty-witness property is not available on
the running kernel, or the mandatory namespace-view monitors cannot be armed,
the facade returns no service clock. Loss of only precision coverage degrades
directory changes to coarse projection; it does not invalidate an otherwise
sound file-only interval.

This focused endpoint returns:

```text
is_fresh_instance = true
clock = clock(B)
files = ["/"]
```

The `/` value is intentionally outside normal relative-path syntax. Current jj
discards `files` whenever `is_fresh_instance` is true and performs a full crawl.
Git's stock Perl adapter ignores `is_fresh_instance`, but copies `/` to Git,
where it is the hook-v2 "invalidate everything" sentinel. Returning a normal
fresh inventory would be unsafe for that adapter: a tracked path deleted during
lost history is absent from the inventory and could retain a valid bit. This
targeted behavior is another reason the endpoint is not advertised as general
Watchman. The sentinel is a control result emitted after query evaluation; it
bypasses normal path validation and expressions and can never arise from an
indexed basename.

The direct Git hook likewise returns `clock(B)\0/\0`. An unknown token is not an
error if a fresh baseline can be established. Invalid requests and transport,
authorization, snapshot, database, or broker failures produce no success
payload:

- jj warns and full-crawls after an ordinary query error; explicit debug and
  baseline-clock commands propagate the error; and
- the Git hook exits nonzero, causing Git to invalidate and verify everything.

The service must never return a successful non-fresh response with a partial or
truncated path set. On a real continuity loss, publication also advances the
replay floor or rotates the watch epoch so other old clocks cannot claim an
incremental result. A merely malformed/foreign clock makes only that request
fresh and does not invalidate valid clients; it still requires the mandatory
view monitors and a new synchronized target boundary.

### 10.6 jj behavior and trigger support

jj's normal request is:

```json
["query", "WATCH_ROOT", {
  "since": "PREVIOUS_CLOCK",
  "expression": ["not", ["anyof",
    ["name", [".git", ".jj"], "wholename"],
    ["dirname", ".git"],
    ["dirname", ".jj"]
  ]],
  "fields": ["name"]
}]
```

`since` is omitted when jj has no saved clock. For a non-fresh response jj
converts each bare name to an exact path matcher; for a fresh response it
ignores the names and crawls the full working copy. It saves the returned plain
string clock only after a successful snapshot with no refused or untracked
paths. If such paths remain, it deliberately keeps the old clock so they are
reported again. An ordinary query error warns, full-crawls, and clears the
saved clock; explicit debug and baseline-clock commands propagate the error.
A changed path must therefore include deletes, both sides of renames, every
hardlink alias affected by object content, and expanded directory moves, and
the server must support repeated queries from the same older clock.

The optional `fsmonitor.watchman.register-snapshot-trigger` setting uses only:

```text
["trigger-list", WATCH_ROOT]
["trigger-del", WATCH_ROOT, "jj-background-monitor"]
["trigger", WATCH_ROOT, {
  "name": "jj-background-monitor",
  "command": ["jj", "--quiet", "util", "snapshot"],
  "expression": JJ_EXPRESSION,
  "stderr": ">/dev/null",
  "stdout": ">/dev/null"
}]
```

V1 accepts only that fixed name, argv, expression, and null redirection. It
rejects `stdin`, `append_files`, `max_files_stdin`, `relative_root`, `chdir`,
shell strings, arbitrary commands, and arbitrary redirection paths. Responses
include the fields required by `watchman_client`: register returns `version`,
`disposition`, and `triggerid`; list reconstructs at least `name` and `command`;
delete returns `version`, `deleted`, and `trigger`. Deleting an absent trigger
is an idempotent success because jj does it during every initialization while
the option is disabled.

Trigger names are scoped to the active grant/front-end principal even though
the index watch is shared. `trigger-list` and `trigger-del` see only that
principal's rows, so one authorized user's default delete cannot remove another
user's background monitor.

The daemon permits `trigger` registration only when its non-root runner was
configured at activation with an absolute `BTRFS_AWACS_JJ` executable. Without
that configuration, `trigger-list` and idempotent `trigger-del` remain
available but registration is an error. The periodic maximum interval is
`BTRFS_AWACS_TRIGGER_INTERVAL_MS` (1000 ms by default, bounded from 10 ms to
one hour). The scheduler claims and validates a durable run while holding the
facade lock, releases that lock before spawning jj so the child can query the
same socket, and reacquires it only to complete the original run fence.

Registering schedules one unconditional run, matching Watchman behavior. While
a trigger exists, periodic synchronized cuts at a configurable maximum interval
are the correctness authority and batch the exact index. The optional precision
guard also wakes the scheduler for low latency and narrows namespace changes to
exact names. The scheduler polls duplicated guard descriptors only for roots
with an active trigger; a terminal delete marker drains the producer's own
barrier events, so marker traffic cannot create a wakeup loop. Dynamic roots
receive independent recursive guards. Losing a guard merely falls back to the
next periodic cut. Each
completed cut evaluates the union of indexed dirty witnesses and any complete
guard events since `last_evaluated_seq`: matching paths advance
`pending_through_seq`; a fresh, mount/path epoch change, unscoped invalidation,
or relevant coarse directory witness schedules an unconditional run. A coarse
directory prefix wholly beneath `.git` or `.jj` is a proven nonmatch for jj's
fixed expression; other coarse prefixes are not. Only a complete nonmatching
range advances the evaluated cursor without running. Exactly one process runs
for a trigger at a time; changes committed while it runs cause one follow-up
run. The durable sequence and run fence make restart retry rather than lose a
transition. Claim order starts with the lowest durable run fence, so a failing
root cannot indefinitely starve another root.

The trigger starts in the exact watched root, with no shell, as the daemon's
unprivileged user. It receives a sanitized environment containing the daemon
socket and standard Watchman root/trigger variables. `.git` and `.jj` are
excluded, so jj updating its own metadata does not recursively trigger itself.
The privileged broker never forks `jj` or interprets PATH/redirection syntax.
`watches.fsmonitor_root` is a raw-byte locator in the fsmonitor owner's recorded
mount namespace, not authority: before every run the user daemon opens it
without symlink traversal and the manager verifies the resulting fd's
FSID/subvolume UUID and active owner grant. If that namespace/path is
unavailable after restart, the trigger remains pending until its owner
reconnects rather than running in the manager's namespace.

The trigger is conservative, not a semantic audit subscription: a transient
write can schedule `jj util snapshot` even when the next cut's final contents
equal the previous cut. That false positive is required because the persistent
dirty witness (or, when complete, its precise journal replacement) also
protects clients which may have observed the transient state.

### 10.7 Native Git hook-v2 adapter

Install a standalone helper and force protocol v2:

```sh
git config core.fsmonitor /usr/libexec/btrfs-awacs/git-fsmonitor-hook
git config core.fsmonitorHookVersion 2
```

Git invokes it from the worktree root as:

```text
git-fsmonitor-hook 2 OLD_TOKEN
```

The helper rejects other versions. It authenticates to the per-user daemon,
resolves or initializes the exact worktree watch, performs a synchronized
query, and writes only this byte protocol to stdout:

```text
NEW_TOKEN NUL PATH NUL PATH NUL ...
```

`NEW_TOKEN` is nonempty. Normal names are nonempty, relative, NUL-free byte
strings. A no-change response is only `NEW_TOKEN NUL`; a trailing NUL after the
last path is allowed. Rename returns both names. Directory moves use trailing
slash prefixes as described above. The helper logs diagnostics only to stderr.

When fsmonitor is first enabled, Git supplies a decimal nanosecond token even
for protocol v2 and has already invalidated all index entries. After a failed
explicit-v2 invocation, Git can supply an empty old token on the next call.
The helper treats numeric, empty, foreign, expired, and otherwise unusable old
tokens as recoverable baseline requests, creates B, and returns
`clock(B) NUL / NUL`. An absent second argv is still malformed invocation
syntax. Returning a valid new token is preferable to an error because Git will
save it while conservatively scanning all paths. If no valid synchronized cut
can be established under the mandatory namespace-view monitors, the helper exits
nonzero and Git performs full verification
without advancing to a service clock.

Each linked Git worktree is a distinct exact-root watch and receives a
watch-scoped token. Copying an index extension from another worktree therefore
produces `/`, not a cross-root incremental answer. Ignored and untracked names
may be over-reported; Git filters tracked entries and invalidates its untracked
directory cache.

The planned optional hardened adapter will use the stock adapter's semantic
query: ask for `fields:["name"]`, exclude only `.git`, and read only `clock`,
`files`, and `error`. On a previously unwatched root it may safely run
`watch`, then a synchronized fresh query, and emit `/`. It must use structured
JSON, list-form process execution, and the normative service-clock alphabet.
The service's fresh `files:["/"]` rule closes the Watchman sample's failure to
inspect `is_fresh_instance`.

The unmodified Perl sample is only a future restricted compatibility test
target for the reasons in Section 10.2. If enabled under that policy, query JSON must
start with `{` at byte zero, diagnostics must remain on stderr, and the
unknown-root error must byte-match the sample's expected expression before it
attempts its `watch` fallback. The direct helper remains preferred because it
avoids two extra processes and JSON/Perl Unicode conversion, supports raw path
bytes, and can use compact Git directory-prefix invalidation.

### 10.8 Baseline and client invalidation rules

A synchronized query takes cut B **before** the client scans returned paths or
performs a fresh crawl. A comparison of only semantic endpoint contents would
not make that ordering safe: after B, a client can remember temporary path `p`,
then `p` can disappear before C and the visible trees can again be equal.

The required persistent dirty witness closes that race without requiring the
optional namespace journal. Snapshot creation commits B as an ordering
barrier. A later create/delete leaves a changed inode item on a surviving
parent directory; a later modify/restore leaves one on the file; and C's
comparison must retain that evidence even if public attributes and net refs
match B. The next query therefore returns an exact alias or a coarse subtree
invalidation. Root-path-binding and mount-topology guards separately rotate the
clock for client-view changes outside the subvolume. A complete optional
journal can replace the coarse directory invalidation with exact names, but it
does not establish the fundamental ordering.

By contrast, calling `clock()` after independently scanning a mutable tree can
still miss a change which lands between the scan and B: that mutation is
already included in B, is absent from the client's older baseline, and need not
appear in B -> C.

Accordingly:

- a general client establishes a baseline with a fresh query followed by its
  crawl, not by crawling and then calling `clock`;
- Worktree may return the child watch's `proved_worktree_seed` sequence-0 clock
  only to the caller which installs revision R as its exact expected-tree
  baseline. The child was cloned from R's snapshot S under the already-armed
  mandatory path/mount monitors, so every later in-root mutation is on the
  S -> first-cut side of the dirty-witness barrier. No validation cut is
  required; a generic caller which did not install R uses a fresh query; and
- `clock`-after-baseline is allowed only while an external mutation exclusion
  is held or when the caller can prove its baseline is exactly the returned
  cut. The current jj `mark_fsmonitor_baseline()` integration must consume the
  proved Worktree seed clock or use the fresh-query protocol.

**Prototype compatibility note.** The current experimental `CHANGED_OBJECTS`
ABI reports a root-directory dirty witness between the original RO seed and
the first cut of its writable clone, even if no post-publication namespace
mutation occurred. That witness is indistinguishable from a real root-level
create/delete after the clock while the precision journal is disabled. The
implementation therefore does not suppress it: on this ABI a Worktree seed
clock is followed by one conservative fresh validation cut, after which the
child watch is incremental. Returning a truly O(1) proved seed without that
cut is gated on the stabilized ABI's explicit dirty-sequence/clone semantics
or a complete mutation-exclusion proof spanning monitor arming and external
rename. The compatibility facade is disabled by default and requires explicit
experimental dirty-witness enablement until that conformance suite passes.

Client-side expected-tree changes also require invalidation. In particular,
jj excludes `.git` and `.jj`, so the service cannot infer that a colocated Git
operation caused jj to replace `TreeState` without touching every worktree
file. Every jj `TreeState::reset`, recovery/import path, and interrupted
checkout which changes the expected baseline must clear its saved Watchman
clock unless it immediately installs the proved Worktree seed clock for that
exact revision.
The currently pinned jj does not clear that clock in every `TreeState::reset`
path, so the integration patch is required before enabling this optimization.
No server projection can repair a stale client baseline while truthfully
returning an empty filesystem delta.

Git owns analogous invalidation of its fsmonitor-valid bitmap when the index is
replaced or fsmonitor is enabled. The provider's watch/store/epoch scoping is a
second guard: a copied, stale, or foreign opaque token yields `/`.

### 10.9 Compatibility transaction and concurrency rules

A normal incremental query is implemented entirely in terms of the existing
transactions:

1. After the mandatory binding/mount checks in Section 10.3, use
   `BEGIN IMMEDIATE` to authorize the exact active fsmonitor-owner grant and
   required permission mask, decode `since`, create or select a `planned` cut
   operation, and insert `cut_admissions(state='waiting')` with the joining
   authorization generation, session, and expiry. The cut worker closes the
   batch in a competing short writer transaction which rechecks the same
   owner/epoch and changes `planned -> fs_started` before the snapshot ioctl.
   An admission can join only while that conditional update has not committed;
   this eliminates the read/check race. The optional precision cursor is not
   captured here; its marker is drained after the snapshot.
2. Changes creates and publishes target cut B and its fsmonitor boundary. All
   admitted waiters receive the same B; later admissions use a later cut. The
   publication transaction fulfills the batch admission rows, but a disconnect
   may merely abandon its own waiter.
3. For each waiter, another short `BEGIN IMMEDIATE` validates its unexpired
   admission/session, exact authorization generation, watch owner, B boundary,
   clock epoch, and replay floor; resolves every immutable adjacent revision
   and comparison; inserts one active `query_leases` row plus all
   `query_revision_pins` and `query_comparison_pins`; and commits. If A was
   reclaimed between admission and this handoff, select a fresh result instead
   of failing correctness. Only when A and B also retain complete cursors in
   one guard epoch above its separate floor does the lease record a precision
   range. GC cannot remove selected history after this transaction, and a
   replacement grant cannot complete work admitted under a revoked UUID.
4. Outside a write transaction, project every pinned adjacent-cut event into
   an immutable per-query result, preserving all directory dirty witnesses,
   expanding aliases and directory moves, and applying the expression. Union
   precision events only for the complete optional range selected in step 3.
   Missing precision history uses coarse projection; missing core history,
   failed/fresh cuts, a relevant coarse witness jj cannot represent, an
   unscoped invalidation, or a budget limit produces the fresh sentinel. Mint
   B's clock only if B is itself a valid boundary under the mandatory view
   monitors and dirty-witness ABI; if it is invalid, produce no success payload
   and retry or fail. If it is valid, encode either the fresh sentinel or the
   filtered, deduplicated paths with B's clock. Long work renews the query lease
   before expiry; failed renewal or a stolen fence discards the private result.
5. Under the grant's shared response phase, drain/recheck the mandatory
   binding and mount monitors and then use a short transaction to verify the
   same active authorization UUID, clock epoch, B boundary, query-lease fence,
   and unexpired lease. Reserve enough renewed lease time for the bounded socket
   write, send the already encoded result, then release the lease and pins in a
   short fenced transaction. Revocation and facade invalidation take the
   exclusive side of this gate: either operation must observe zero active
   response leases or roll back and retry after they drain. They never delete a
   live response's pins or rotate its epoch underneath an in-progress write.
   The transport has a write deadline shorter than the reserved lease, and
   releases the lease on success, timeout, disconnect, or encoding failure. If
   any final check fails, send no stale bytes and restart with a fresh boundary. Trigger
   evaluation uses the same pinning protocol.
   Publishing B durably makes it newer than the trigger's
   `last_evaluated_seq`; evaluation either advances that cursor for a proven
   complete nonmatch or advances `pending_through_seq` for a match or any
   coarse/full/fresh uncertainty. Runner claim and completion use separate
   short fenced transactions.

Concurrent callers with different old clocks can therefore share a target cut
without sharing mutable result state. SQLite still has one short writer at a
time; WAL readers, kernel comparisons, path projection, client encoding, and
trigger execution run concurrently. A failed caller cannot roll back a cut
already shared or published for another caller. Writer contention serializes
only admission, publication, pin, and release metadata, not the expensive
filesystem or projection work.

### 10.10 Compatibility acceptance tests

In addition to the core index tests, record byte-exact fixtures for jj's BSER
discovery and six commands, including bare `NameOnly` values and the initial
fresh `/` result. Run jj status/snapshot tests for create, delete, rename,
hardlinks, `.gitignore`, `.git`/`.jj` exclusion, sparse matches, directory
subtree moves, daemon restart, expired clocks, query failure, and the Worktree
proved-seed baseline. Include repeated old-clock queries when refused/untracked
paths prevent persistence; a regression where jj resets its expected tree
without ordinary file events; and non-UTF-8 results which must cause the pinned
client's query error followed by its currently failing full crawl until both
byte-path patches land. No cleared clock may be persisted after that failure.

Run Git's fsmonitor tests against the direct hook for its initial numeric token,
empty token after a fail-once/index-write cycle, empty delta, create/delete,
both rename names, non-UTF-8 bytes, directory prefix, linked worktrees,
copied/foreign tokens, restart, GC expiration, `/` fallback, and hook failure.
Run the bundled hardened adapter against the JSON shim. Separately use the
unmodified Perl sample only under its restricted policy, with byte-exact tests
that query output starts with `{`, activation diagnostics are stderr-only, the
error contains `unable to resolve root ... directory ... is not watched`, and
`watch`/`clock` output and every service token satisfy the required JSON-safe
alphabet. Include a lost-history deletion proving `files:["/"]` is required.

The critical dirty-witness suite runs first with the optional precision guard
disabled. Pause clients after B's clock, then create/delete a file or whole
subtree, modify/restore data and metadata (including writable `mmap`), mutate
through one hardlink while another alias is cached, and perform root-level and
nested namespace operations before C. Every case must leave an object or
surviving-directory witness and project an exact alias, Git prefix, or jj fresh
result even when B and C's public snapshot states compare equal. Include the
mixed `d/transient` create/delete plus `d/persistent` net-create case and prove
the known ref does not erase `d`'s coarse witness. Run each mutation around the
snapshot ioctl/transaction barrier. Kernel versions which fail any witness
test must refuse to mint facade clocks.

Then enable the optional precision guard and prove exact root-level and nested
names replace coarse directory fallback only between two post-snapshot marker
boundaries in one retained epoch. Exercise directory create/move-in scoped
prefixes, queue overflow, producer restart, watch-install races, permission
loss, unresolved/reused inodes, and hardlink alias-expansion failure. Each
failure must gap precision and use a scoped/full or snapshot-only coarse result,
never a partial or empty success.

The mandatory namespace-view suite renames/replaces and restores the watched
root and each ancestor component between queries; attaches/detaches bind, FUSE,
and moved mounts; loses each monitor fd; restarts the namespace daemon; and
changes `chroot`/mount namespaces on passed sockets. Binding markers must catch
path operations, a mountinfo event must rotate every watch using that namespace
monitor even if its text returns identical, and persistent replacements must
also fail exact UUID/mount re-resolution. Include PID exit/reuse and mixed or
missing `SCM_CREDENTIALS`/`SCM_PIDFD` spans. Confirm explicitly that a same-UID,
same-view process can use a passed socket under v1's trust model rather than
mistaking Landlock/seccomp restrictions for delegated authorization.

Stress tests coalesce simultaneous `clock`/`query` callers, kill workers before
and after snapshot and SQLite publication, reclaim trigger leases, overflow
every output limit, race admission against `fs_started`, revoke/regrant during
each operation phase, race GC against projected queries, connect a same-UID
client from another mount namespace, expire/steal query leases during encoding,
and race revocation against final response and broker dispatch. Verify that
admission history reclaimed before pin handoff becomes fresh, a running broker
receipt is reconciled before revocation completes, and every uncertainty is
either a complete incremental set or a full invalidation—never a partial
successful result. Trigger tests prove that matching transients schedule a run,
coarse/full/fresh ranges run unconditionally, and only proven nonmatches advance
the evaluation cursor without running. Worktree tests arm binding/mount
monitors before the external rename, validate the expected move event and exact
UUID, reject unauthorized metadata-relocation policies, and prove a returned
sequence-0 clock is accepted only with the exact seed revision. Attempt dedupe
against managed RO cuts, force boot/unclean-restart boundaries, and verify
transaction-metadata or epoch checks prevent stale publication.

As a minimum real-client smoke gate, boot the UML image, record `jj --version`
and `git --version`, run an unmodified jj working-copy status through socket
discovery, `watch-project`, and the name-only query, and run an unmodified Git
status through the direct v2 hook before and after editing a tracked file. With
jj's fixed trigger enabled, configure a five-second periodic maximum, mutate
only after the scheduler is waiting, and require a completed snapshot within
two seconds; this proves the precision fd is an early-wakeup mechanism rather
than merely observing the next periodic cut. The current recorded smoke pair is
jj `0.43.0-28e25c32bc98b6cfba430b4fa44f86141e94266a` and Git `2.43.0`.

## 11. Implementation plan

1. **Freeze and test the contracts.** Land this document plus fixtures for
   create/delete, data and metadata changes, hardlink aliases, link/rename,
   directory subtree moves, inode reuse, raw non-UTF-8 names, nested-subvolume
   rejection, historical comparisons, and cursor gaps. Update the profiling
   harness to use the current `snap`/`compare` CLI before relying on its timing
   mode.
2. **Refactor the Rust parser.** Turn the summary-only changed-object parser
   into a streaming typed parser which preserves records and rejects unknown
   semantics. Separate `btrfs`, `manifest`, `index`, `store`, `events`, and CLI
   layers. Add direct safe wrappers for FS/subvolume info.
3. **Introduce the broker boundary.** Define a narrow Unix-socket protocol,
   fd passing, durable watch grants and immutable Worktree policies,
   destination-parent identity checks, the root-owned execution-receipt
   journal and revocation/dispatch gate, best-effort current limits, and fixed
   operations. Remove general `sudo btrfs` shelling from the service path; keep
   benchmark commands available separately.
4. **Add SQLite and migrations.** Implement the schema, BLOB encodings, WAL
   configuration, operation intents, watch/topology leases and fences,
   fence-specific spool/staging files, immutable-revision lookup, transactional
   pin handoff, terminal grants and policy generations, cut admissions,
   fsmonitor boundaries, optional mutation events, query leases/pins, terminal
   aborts, revocable retention leases, fenced snapshot-delete intents, and
   startup reconciliation. Add process-concurrency and
   grant-revocation tests.
5. **Implement Initialize.** Create the external RO cut and build an exact
   checkpoint with an fd-relative userspace namespace walk. During transition,
   compare it with the privileged tree-search prototype. Reject top-level,
   manager-ancestor, and nested-root sources based on the immutable cut.
6. **Implement delta application.** Spool and identity-check A -> B, normalize
   packed refs, stage overlays, derive paths and hardlink aliases, validate the
   immutable cut's no-nested-boundary invariant and the graph, maintain the
   owner/privileged-metadata Worktree safety summary, persist events, and CAS
   the head. Property-test
   `apply(full(A), delta(A,B)) == full(B)`.
7. **Implement concurrent Changes.** Add per-watch cut leases, comparison-job
   deduplication, ordered publication, deterministic retry/replay, direct
   same-branch historical A -> B comparisons, and the full-checkpoint
   gap/fresh-instance transaction.
8. **Prove the dirty-witness foundation before exposing clocks.** Make every
   inode-item change, especially directory witnesses, survive normalization and
   projection. Add the snapshot-only post-clock race/xfstest matrix for every
   supported mutation mechanism and refuse facade activation on a kernel/ABI
   which fails it. Implement exact aliases for file changes and conservative
   Git-prefix/jj-fresh projection for every changed directory.
9. **Add namespace continuity, clocks, and query transactions.** Build the
   per-user daemon with pidfd-bound per-frame credentials, mandatory top-down
   root-path-binding watches, the persistent mountinfo monitor, component/UUID
   re-resolution, and epoch rotation. Then implement authenticated
   watch/grant/view-scoped clocks, writer-serialized cut admission/coalescing,
   the cut replay floor, renewable query leases/pins, adjacent-cut witness
   aggregation, client-specific fresh results, final response fencing, and
   byte-oriented limits. Property-test that every result is a conservative
   superset of state a client could have cached after its previous clock.
10. **Add the optional precision journal.** Implement recursive inotify on a
    separate fd, top-down arming, post-snapshot private markers, durable cursor
    assignment, exact create/delete/rename names, scoped directory-create and
    move-in prefixes, guard retention/floors, and coarse fallback on every gap.
    Benchmark how often this converts jj fresh crawls into exact incremental
    matches; correctness must continue to pass with it disabled.
11. **Add jj's focused Watchman endpoint.** Implement bounded BSER-v2 framing,
   `watch-project`, `clock`, name-only `query`, exact expressions, fresh `/`
   semantics, namespace-specific discovery, peer namespace/grant checks, and
   byte-exact fixtures captured from `watchman_client` 0.9.0. Patch jj to
   expose raw byte paths in both Watchman and crawl paths, preserve its
   refused/untracked clock behavior, clear clocks on every expected-tree reset,
   and consume only fresh-query or proved Worktree-seed baselines.
12. **Add Git fsmonitor.** Implement the direct hook-v2 helper first, including
    numeric/empty/foreign-token initialization, NUL-safe names, compact
    directory invalidation, and nonzero failure fallback. Add the structured,
    no-shell hardened JSON adapter and focused three-command CLI shim. Treat the
    unmodified Perl sample only as a restricted conformance target and run
    Git's hook/fsmonitor suite plus the lost-history deletion regression.
13. **Implement Worktree branching.** Clone a published RO anchor to a
   same-filesystem authorized destination with `RENAME_NOREPLACE`, reject
   descendants of every non-deleted watch, verify the caller reservation plus
   immutable metadata-relocation policy, and share the seed revision pointer.
   Arm mandatory destination binding/mount monitors before external rename and
   return the proved sequence-0 clock only to the caller installing the exact
   seed revision. Exercise jj integration and every crash/revocation point.
14. **Add optional jj triggers.** Persist only the fixed jj trigger, use
    periodic cuts as the correctness cadence and precision activity for early
    wakeups, treat relevant coarse/fresh/full uncertainty as an unconditional
    match, add fenced non-overlapping execution and pending reruns, and run it
    through the unprivileged sanitized namespace owner. Keep all command
    execution outside the broker.
15. **Implement GC/recovery.** Add physical pins, two-phase snapshot deletion,
   filesystem commit barriers, revision/event retention, checkpoint compaction,
   independent cut/precision-floor advancement, query/admission lease
   reclamation, retention expiry/revocation, grant-generation and
   reservation-aware fd reauthorization,
   root-owned broker-receipt reconciliation before fence takeover,
   view-monitor handoff rules, orphan and trigger-run reconciliation, and fault
   injection at every filesystem/SQLite boundary.
16. **Stabilize the kernel ABI.** Document v2 structs, add fd-anchored roots,
   stream identities/footer, inode-only change masks including
   `BTRFS_CHANGED_OBJECT_CHANGE_FILE_DATA` and
   `BTRFS_CHANGED_OBJECT_CHANGE_DIR_ENTRIES`, an explicit monotonic dirty
   sequence, and nested-boundary
   semantics; keep full-index traversal in userspace;
   add the optional per-subvolume precise mutation-journal
   ABI, Btrfs/xfstests, and parser fuzzing. Keep the broker even if a later
   kernel authorization model permits selected unprivileged watches; replace
   inotify only after the kernel journal passes the same post-clock race suite.
17. **Validate performance and correctness.** Benchmark initialization,
    snapshot latency, kernel comparison, SQLite application, precision ingestion,
    path/alias expansion, BSER/hook projection, trigger cadence, and GC
    separately. Test direct A -> C final state against A -> B -> C and test
    post-clock transient activity with precision disabled and enabled.
    Exercise simultaneous callers, killed workers, disk-full SQLite, truncated
    manifests, snapshot deletion races, large hardlink sets, clock expiration,
    namespace/grant changes, and every full-invalidation path.

## 12. Current prototype mapping

The benchmark `snap` and `compare` commands remain available, but the service
prototype no longer reuses their in-tree layout or summary parser as its state
model. The Rust implementation now includes:

- fd-anchored FS/subvolume inspection and a local dedicated changed-objects v2
  ioctl for deltas only. The broker passes source/target root
  fds, and userspace verifies endpoint identities, completion counts, CRC32C,
  exact target inode/security-xattr records, mandatory nested-subvolume
  boundary coverage, and the private-spool SHA-256 before applying them.
  `DIR_INDEX` root entries become boundary add/delete records; a boundary-free
  base plus no effective add is an incremental proof, while any add rejects the
  cut without advancing its indexed sequence. Full indexes and authoritative
  target-object rows are also checked for fscrypt state before publication, so
  encryption cannot enter an accepted revision merely because no facade is
  active. Kernels returning `ENOTTY` retain the legacy private
  send flag plus privileged tree-search full/target reads; other v2 failures
  never silently fall back;
- a fixed binary `SOCK_SEQPACKET` broker protocol with `SCM_RIGHTS`, peer-UID
  authentication, session fencing, bounded frames, private output files, and
  a root-owned receipt journal for snapshot create/delete and Worktree rename.
  Worktree publication is rooted at a broker-verified policy-subvolume fd,
  uses `openat2` beneath resolution, rejects idmapped mounts through
  `statmount`, and binds directory security-xattr hashes into the receipt;
- a standalone `broker-serve` deployment path. The UML acceptance test runs
  the manager through this external broker; an embedded socketpair dispatcher
  remains available only for tests and explicitly selected prototype callers;
- the normative manager SQLite schema, immutable checkpoints and overlays,
  fenced operations, snapshot pins, two-phase physical GC, expiring retention
  leases, grant revocation cleanup, broker drain plus manager-owner handoff,
  exact stale-spool cleanup, unexpected-object quarantine, restart
  invalidation, and tracked Worktree branches which share their immutable seed
  revision;
- authenticated clocks, query leases, conservative jj/Git projection, binding
  and mount-namespace continuity monitors, focused BSER-v2 Watchman commands,
  the Git hook-v2 byte protocol, response leases held through bounded daemon
  socket writes (with a five-second deadline and release on timeout/failure),
  facade-lock release while the pinned frame is written, pre-construction
  result-byte/item caps with fresh `/` fallback, durable fixed-trigger
  registration, periodic synchronized trigger cuts, and fenced non-shell jj
  execution outside the facade lock; a READ-authorized historical replay API
  resolves retained snapshot UUIDs to one ordered watch branch under a SQLite
  read snapshot, concatenates every retained adjacent witness in cut/ordinal
  order, and returns `fresh_instance` with no partial event stream if the
  replay floor or a missing comparison breaks the interval; and
- terminal delta failure and checkpoint-gap recovery: a failed cut is fenced
  and releases only its operation pins; a later validated immutable target can
  publish a `full_fresh` comparison plus complete checkpoint only after one
  transaction proves every skipped sequence terminally failed and CASes the
  old indexed head. The service also uses this safe full-index path immediately
  when the experimental changed-object comparison, parser, or target-object
  lookup fails for the current immutable cut; and
- a fence-named durable changed-object stage whose completion trailer binds the
  exact byte length and SHA-256 and is fsynced only after broker success.
  Restart revalidates and reuses a complete stage, discards a partial one, then
  deterministically rebuilds connection-private parsed TEMP rows for the
  fenced canonical import; and
- direct retained A -> B comparison jobs for gaps outside the adjacent replay
  window. READ authorization and a lease fence serialize one algorithm-v2 job;
  both immutable snapshots are pinned, the kernel delta is applied to indexed
  A, and publication is refused unless the result exactly equals already
  indexed B. The cached witness comparison never mutates the watch head; and
- concurrent daemon query preparation: authorization and namespace checks run
  in short facade-lock phases, while each cut worker opens its own SQLite
  connection and joins (without rotating) the current authenticated broker
  session. Snapshot/ioctl/index work runs outside the facade lock; same-watch
  callers coalesce through `cut_admissions`, and final boundary insertion is
  idempotent for every waiter before each receives its own response lease; and
- a namespace-specific `watchman --output-encoding bser-v2 get-sockname` shim,
  locked automatic daemon activation, authenticated dynamic exact-root
  registration (including existing Worktree watches), a Git hook fallback to
  the same discovered socket, installable entry-point symlinks, and a hardened
  root broker systemd unit; and
- a pre-publication Worktree view handoff: the manager arms the destination
  parent while the final basename is absent, accepts exactly the broker's
  expected move-in, proves the mount namespace/process root and generated
  Btrfs UUID remained bound, activates and records the sequence-0 boundary,
  then transfers the still-live monitor into the facade. A restart, unexpected
  event, monitor-arm failure, or caller which does not consume that exact
  handoff cannot mint the seed clock and must establish a fresh baseline.
  Canonical Worktree locators participate in the per-filesystem topology
  lease: reservation rejects a destination beneath every initializing,
  active, or blocked watch; Initialize symmetrically rejects a source which
  contains any creating, present, or deleting Worktree; and recovery or the
  live path reacquires and holds the same exclusion from the final ancestry
  recheck through broker rename and SQLite publication; and
- an opt-in recursive inotify precision producer (`BTRFS_AWACS_PRECISION_GUARD=1`)
  with a disjoint private marker directory, transactional event/head updates,
  complete boundary cursors, query-lease precision ranges, scoped new-directory
  fallback, independent guard-floor reclamation, and durable coarse fallback
  on overflow, watch loss, marker failure, or producer ambiguity; and
- UML coverage on the modified kernel for initialization, incremental cuts,
  hardlink aliases, transient dirty-witness fallback with the precision guard
  both disabled and enabled, external broker
  operations, Worktree publication/tracking, GC, Watchman, Git, and triggers.
  The same acceptance boot also runs an unmodified jj binary through its
  Watchman backend and periodic snapshot trigger and a real Git binary through
  the direct hook-v2 fsmonitor helper; it records both client versions.
  The guard-disabled matrix separately covers a create/delete file, a wholly
  transient subtree, mixed nested transient plus persistent creation,
  hardlink data modify/restore, mode modify/restore, and writable `mmap`
  modify/restore; every case yields either the required coarse ancestor witness
  or both hardlink aliases.

The current kernel ABI still reports a root dirty witness for the first cut of
a writable clone, so the prototype deliberately returns one fresh validation
cut instead of claiming a proved O(1) seed clock. The recursive precision
journal is optional and disabled by default; without it, the snapshot-only
facade returns conservative `/` invalidations when the kernel witness cannot
prove that no transient was observed. With it enabled, only two retained
complete cursors in one epoch can replace those directory witnesses; every gap
returns immediately to the same coarse behavior. Upstream stabilization of
kernel ABI v2, the full dirty-witness xfstest matrix, the optional JSON
adapter, system distribution-specific
package metadata, hardlink-aware object notification in a future kernel
journal, a wider real-client version matrix, and the full fault/performance
matrix remain stabilization work rather than silently weakened behavior.

Delta publication resolves a change-closed slice of
the base overlay chain—changed objects, every hardlink alias, collision
candidates, and directory ancestors—and maintains counts, security summaries,
and owner cardinalities by composable deltas; it no longer materializes the
unrelated namespace. Canonical child rows are constructed in file-backed,
connection-private TEMP staging and bulk-imported only after the publication
fence succeeds; their authoritative input is now the crash-resumable
fence-named manifest above. Persisting the already-parsed canonical rows as a
directly attachable staging SQLite database would avoid reparsing after restart
but is a throughput optimization, not a recovery or atomicity gap. The
remaining limitations affect throughput or force conservative fresh
validation; they are not grounds for a partial incremental response.

## References

- [Btrfs subvolume documentation](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html)
- [Btrfs ioctl documentation](https://btrfs.readthedocs.io/en/stable/btrfs-ioctl.html)
- [Btrfs mount options (`user_subvol_rm_allowed`)](https://btrfs.readthedocs.io/en/latest/ch-mount-options.html)
- [SQLite write-ahead logging](https://sqlite.org/wal.html)
- [Watchman `query`](https://facebook.github.io/watchman/docs/cmd/query)
- [Watchman clocks](https://facebook.github.io/watchman/docs/clockspec)
- [Watchman triggers](https://facebook.github.io/watchman/docs/cmd/trigger)
- [Git `fsmonitor-watchman` hook](https://git-scm.com/docs/githooks#_fsmonitor_watchman)
