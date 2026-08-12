# Indexed Btrfs change tracking

Status: normative v1 design plus an experimental implementation. The
`btrfs-awacs` binary retains its `snap` and `compare` benchmark commands and
also implements the manager, privileged broker, persistent index, and direct
immutable-snapshot scan endpoint described below.
Production garbage collection is unavailable.
Section 12 distinguishes implemented behavior from remaining stabilization
and performance work.

## 1. Goals

The service maintains a persistent namespace index for immutable Btrfs
snapshots. Its design provides three operations:

1. **Initialize** a writable or read-only subvolume by taking a read-only
   snapshot and building a complete index of that snapshot.
2. **Changes** by taking another read-only snapshot, updating the index from a
   kernel object delta, and returning a durable stream of changed names.
3. **Garbage collection** of managed snapshots and index history which are no
   longer reachable or pinned. Existing maintenance scaffolding is not wired
   into production.

Snapshot creation shares Btrfs metadata rather than walking file contents.
Index creation is necessarily O(namespace size). An incremental update should
be O(changed B-tree items + changed references + output), subject to the
directory-rename caveat below. "O(1) snapshot" refers to the logical
copy-on-write snapshot operation, not a bound on transaction commit latency or
future copy-on-write allocation.

When a separately created Btrfs snapshot descendant is registered, the service
can adopt its retained parent snapshot revision without copying all index rows.
The service does not create, publish, or manage writable descendants.

The supported client boundary is a private direct-scan protocol. It projects
the durable comparison stream into conservative invalidations and transfers a
leased descriptor for the exact immutable snapshot the client must read.

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
  which permits an existing snapshot descendant to adopt its parent's indexed
  revision.

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
direct invalidation result.

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
stream. A client can observe additional mutations after receiving a snapshot
clock while it crawls or updates its cached tree. A later result must cover
every cached path which might therefore differ, not merely the semantic
differences between immutable snapshot endpoints.

V1 requires a **persistent dirty-witness** invariant from the kernel ABI. RO
snapshot creation is a transaction barrier. Every later client-visible
mutation must either change an emitted item for the affected surviving inode,
or persistently change an emitted inode item for a surviving parent directory
(and therefore the nearest surviving ancestor of a wholly transient subtree).
The current prototype appears to have this property because Btrfs snapshot
creation commits a transaction, inode-item updates store its `transid` and
sequence, and `CHANGED_OBJECTS` deep-compares those items. This observation
must become a documented ABI promise and an xfstest suite before production.

The client projection must retain changed-directory witnesses until it can
invalidate the affected subtree conservatively. Git can receive a recursive
directory prefix; jj needs either bounded exact-path expansion, a proven
complete precision-journal interval, or a fresh/full-invalidation response.
An endpoint-equal transient may be omitted only when an independent baseline
proof establishes that the client could not have cached it. The optional
durable namespace guard can turn coarse invalidations into exact names, but
correctness must also hold when the guard is absent or gapped.

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
    quarantine/                         # unexpected managed-looking objects

/var/lib/btrfs-awacs/broker/            # root-owned, not writable by manager
    receipts.sqlite3                    # privileged-execution journal
```

The privileged broker and each user's scan daemon use different
sockets and different trust domains:

```text
/run/btrfs-awacs/
    service.sock                        # manager-owned, grant-checked API
    broker.sock                         # root-owned, manager access only

$XDG_RUNTIME_DIR/btrfs-awacs/           # mode 0700, owned by one user
    mnt-<namespace-dev>-<namespace-ino>/
        scan.sock                       # mode 0600, private seqpacket endpoint
        daemon.lock
```

The per-user daemon runs in one mount namespace, interprets working-tree
paths, and serves direct-scan clients. It sends
fd-anchored, grant-checked requests to the unprivileged index manager, which
owns SQLite, snapshots, spool files, clocks, and cut coordination. Only that manager
can request fixed Btrfs operations from the root broker. The broker does not
parse client scan packets or execute client commands. A system-wide scan socket
would need fd-passing root registration and a real user/process sandbox; v1
does not provide one.

A UID may have processes in several mount namespaces. Discovery keys the socket
by the caller's mount-namespace identity, and the daemon independently compares
its namespace with the connecting peer's namespace and process root using
kernel-supplied Unix peer credentials plus `/proc/<pid>/ns/mnt` and
`/proc/<pid>/root`. A shared mount namespace is still insufficient because a
process may have a different `fs_struct` root after `chroot`. The daemon
rechecks the sender's current UID, mount namespace, and process-root identity
before serving the private scan connection. This catches post-connect
`setns`/`chroot` changes and makes socket passing attributable to the actual
sender rather than treating connection setup as permanent authority. It does
**not** preserve a same-UID process's
narrower Landlock, seccomp, chroot, or LSM policy: v1 deliberately treats all
same-UID processes in the recorded namespace/view as one trusted principal,
and the socket fd is delegable within that principal. A deployment which must
isolate same-UID sandboxes needs separate service principals or a kernel/MAC
capability bound to the narrower security domain; mode `0600` and
`SO_PEERCRED` are not such a boundary. An explicit scan socket does not bypass
the UID/view checks.

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

The prototype's current `<source>/.btrfs-awacs` layout must not be used for the
service. It mutates the watched namespace, creates nested-subvolume stubs in
later snapshots, and makes lexical timestamp order stand in for durable state.

## 5. Kernel interfaces

### 5.1 Existing interfaces

| Purpose | Interface | Requirements and use |
| --- | --- | --- |
| Identify filesystem | `BTRFS_IOC_FS_INFO` | Read FSID after opening the root. No special capability beyond access to the fd. |
| Identify subvolume | `BTRFS_IOC_GET_SUBVOL_INFO` | Read UUID, parent/received UUID, root ID, and transaction metadata. Recheck after every create and before comparison/publication. |
| Create RO cut | `BTRFS_IOC_SNAP_CREATE_V2` | Destination directory fd, source-root fd, and `BTRFS_SUBVOL_RDONLY`. Both paths must be on one Btrfs filesystem. Snapshot creation commits a filesystem transaction; the fsmonitor design relies on that ordering barrier. |
| Incremental object delta | Local `BTRFS_IOC_CHANGED_OBJECTS` v2; legacy fallback is experimental `BTRFS_IOC_SEND` with exactly `NO_FILE_DATA` plus `CHANGED_OBJECTS` | V2 receives source and target root fds, requires distinct RO roots on one filesystem, and emits endpoint identities, target attributes/xattrs, nested-boundary transitions, bounded records, and a checksummed completion footer. The legacy parent is a numeric root ID and is accepted only when the dedicated ioctl returns `ENOTTY`. Neither local extension is upstream ABI. |
| Initial exact index | Userspace traversal of the immutable RO snapshot | Enumerate raw directory entries and obtain each reachable object's metadata through ordinary fd-relative VFS operations. The prototype still uses privileged `BTRFS_IOC_TREE_SEARCH_V2` while the VFS walker is implemented; `BTRFS_IOC_CHANGED_OBJECTS` has no full-index mode. |
| Root-path-binding continuity | A separate `inotify_init1` fd watches every parent/component from the pinned process root to the watched subvolume root | Mandatory for clocks unless an equivalent immutable-path policy is enforced. Arm top-down before resolving the next component; relevant create/delete/move/self/ignored events, overflow, unmount, permission loss, or monitor restart rotate the clock epoch. Drain a private marker and re-resolve the complete inode/mount/UUID chain at admission and final response. This fd is separate from the optional recursive precision guard so subtree load cannot silently weaken authority. |
| Optional precise namespace guard | Recursive `inotify_init1` / `inotify_add_watch` in the per-user daemon | Durably records exact create/delete/rename names for client projection and diagnostics. It requires traversal/watch access to the whole working tree and does not observe every content mechanism (for example writable `mmap`). Without a complete guard interval, surviving directory witnesses require conservative Git-prefix or jj-fresh invalidation. |
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
  replay/read, caller-directed retention, and watch-deletion request rechecks
  it. Physical snapshot GC is a separately fenced manager-policy operation
  after all pins are gone;
- permits only fixed snapshot, full-index, changed-object, and deletion
  operations on managed roots and never follows a caller-supplied path string;
- serializes an execution receipt per `(operation ID, target identity)` around
  every privileged filesystem mutation in its root-owned receipt journal.
  Dispatch takes the grant's execution gate, rechecks authorization/fence,
  records the request, and has the broker durably mark it running before the
  ioctl. Revocation takes that gate exclusively: if revocation wins, dispatch
  cannot start; if dispatch wins, revocation records pending and orders after
  exact completion or broker-journal reconciliation. The manager may not
  expire, transfer, abort, or issue a conflicting fence while the receipt is
  running. This is essential for snapshot creation and deletion: a DB lease
  fence can prevent stale publication but cannot stop an already-started ioctl;
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
| Current changed-object delta | Requires `CAP_SYS_ADMIN`; invoke through the broker. |
| Exact initial `TREE_SEARCH_V2` index | Requires `CAP_SYS_ADMIN`; invoke through the broker. A normal directory crawl is permission-filtered and lacks the exact Btrfs generation data required by the durable model. |
| Managed snapshot deletion | Normally `CAP_SYS_ADMIN`. Unprivileged removal is possible only with the `user_subvol_rm_allowed` mount option plus the relevant directory permissions. Use the broker by default. |
| SQLite reads/writes and path derivation | No kernel capability; Unix permissions on the manager store apply. |
| Mandatory root-path-binding monitor | No capability when the per-user daemon can read/watch every ancestor directory in its own view. V1 disables the scan facade if any component cannot be watched or inotify coherence is not trusted; the core snapshot API remains available. A privileged notification-only replacement would need a separately constrained broker protocol. |
| Optional recursive inotify precision guard | No capability when the per-user daemon can traverse/watch the whole working tree. If it cannot establish and retain complete coverage, scans remain available through conservative snapshot-only invalidation, but exact transient names are unavailable. |
| Mount-topology continuity monitor | Polling an already-open `/proc/self/mountinfo` fd requires no capability and observes the daemon's mount namespace. If the optional `FAN_MARK_MNTNS` interface is used instead, it requires `CAP_SYS_ADMIN` in the fanotify group's user namespace and the broker supplies only a notification-class, mount-event-only fd. |
| Direct immutable scan | Runs in the per-user daemon without capabilities. It checks each connection's actual sender and passes a rooted fd plus view binding to the manager; the manager authenticates that daemon as its service peer and checks the watch grant before asking the broker for a cut. |

The per-user runtime directory is opened without following symlinks and checked
for the expected owner and mode. Stale-socket replacement verifies the existing
entry's type, owner, and inode before unlinking it. The daemon obtains peer
credentials from the connected Unix socket; a cursor or root path is not a
bearer capability. Packet bytes, result paths, waiters, cuts, and comparisons
all have explicit limits. A semantic result which exceeds its path budget
becomes a conservative full invalidation; a malformed or oversized transport
request is rejected.

Each grant has one ordering gate for response and privileged dispatch.
Ordinary projection runs without it; the query takes a shared response phase
only for its final fenced authorization check and bounded nonblocking write.
The frame has a fixed byte cap/deadline; on timeout the daemon closes the
connection so a partial frame is unusable before releasing the gate. A broker
dispatch takes the shared execution phase through the durable-start handshake
above. Revocation takes the exclusive phase, then atomically revokes the grant
and fences its admissions, query leases, and operations. It therefore
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
same authorization policy. Registering an existing snapshot descendant also
requires its own explicit grant; knowing a parent's watch ID or sharing its
index does not grant access.

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
columns and indexes without changing the invariants. `src/store.rs` extracts
the SQL blocks directly from this document.

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
    -- 0x01 READ, 0x02 CUT, 0x10 RETAIN, 0x20 ADMIN.
    -- Unknown bits are rejected.
    permissions     INTEGER NOT NULL CHECK
                    (permissions > 0 AND (permissions & ~51) = 0),
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

CREATE TABLE operations (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    kind            TEXT NOT NULL CHECK (kind IN ('initialize', 'cut')),
    state           TEXT NOT NULL CHECK
                    (state IN ('planned', 'fs_started', 'fs_created', 'uuid_recorded',
                               'manifest_ready', 'index_committed',
                               'done', 'failed')),
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
    reserved_path   BLOB NOT NULL,
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
    CHECK ((guard_epoch IS NULL) = (guard_sequence IS NULL))
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
                    (request_kind IN ('clock', 'query')),
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

-- One committed external-clock boundary per fsmonitor-visible cut. Guard fields are an
-- optional precision cursor, not part of the coarse dirty-witness proof.
CREATE TABLE fsmonitor_boundaries (
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    cut_sequence    INTEGER NOT NULL CHECK (cut_sequence > 0),
    target_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    boundary_kind   TEXT NOT NULL CHECK (boundary_kind = 'cut'),
    cut_operation_id BLOB NOT NULL,
    clock_epoch     BLOB NOT NULL CHECK (length(clock_epoch) = 16),
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    guard_sequence  INTEGER CHECK (guard_sequence IS NULL OR guard_sequence >= 0),
    guard_complete  INTEGER NOT NULL CHECK (guard_complete IN (0, 1)),
    PRIMARY KEY (watch_id, cut_sequence),
    UNIQUE (watch_id, target_snapshot_id),
    FOREIGN KEY (watch_id, cut_sequence, target_snapshot_id, cut_operation_id)
        REFERENCES watch_cuts(watch_id, sequence,
                              target_snapshot_id, operation_id),
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
                    (effect_kind IN ('snapshot-create', 'snapshot-delete')),
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
    opcode          INTEGER NOT NULL CHECK (opcode IN (3, 5)),
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
request hash covers every fixed argument, authorization generation, target
locator, and expected identity. It records the generated UUID/result before
`completed`. A crash which leaves
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

Every ready revision does require its incrementally maintained ownership and
security summary: `single_owner_uid` is the common owner or NULL for mixed ownership,
`privileged_metadata_count` counts any set-ID/device/capability/disallowed
security metadata, and `security_state_hash` commits to the per-object
classification. `privilege_flags` and the security-xattr digest are derived by
the privileged full-index/targeted-lookup path, not trusted from a caller. This
metadata is not exposed in jj/Git change results.

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
`RETAIN=0x10`, and `ADMIN=0x20`. Historical
read/replay requires `READ`; `Changes`, `clock`, and `query` require
`READ|CUT`; caller-controlled retention requires `RETAIN`; and watch deletion
or caller-directed GC requires `ADMIN`. Automatic policy GC runs as the
manager rather than borrowing a caller bit. A broker request carries the exact
authorization generation and required mask; unknown bits and a merely
overlapping mask are rejected.

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
fences every waiting admission, active query lease, and unstarted operation
tied to the old grant, atomically terminates its retention leases and
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

For an ordinary adjacent cut, `watch_cuts.comparison_from_snapshot_id` equals
`base_snapshot_id`. They differ only for the explicitly fenced Gap recovery
transaction, where the former names the actual older indexed head and the
latter retains the immediate physical-cut lineage.

Application code enforces graph invariants which cannot be expressed as simple
foreign keys across overlays:

- every ready revision belongs to exactly one immutable RO snapshot, and every
  storage base, comparison endpoint, operation base, watch head/cut, and
  boundary edge stays on that same recorded filesystem and watch branch;
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
- every sequence above a watch's replay floor through its indexed head is a
  complete ready cut with no unmarked gap;
- every fsmonitor boundary names its exact ready
  `(watch, sequence, snapshot, operation)` tuple. An optional guard cursor is
  in the boundary's complete precision epoch. The single fsmonitor owner
  refers to an active grant for that watch;
- every active query lease pins every comparison/revision it may read and
  prevents guard-event GC through any recorded range, and its authorization
  generation and clock epoch still match before response; and
- every active caller `retention_leases` row belongs to an active grant with
  `RETAIN`, names a snapshot on that watch branch, and has exactly one matching
  `snapshot_pins(owner_kind='retention-lease', owner_id=lease.id)` row; terminal
  or expired retention rows have none.

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
2. Acquire the per-filesystem topology lease. Recheck that the source does not
   contain the manager store.
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
   represented by the watch heads and has no `watch_cuts` row. An already
   existing snapshot descendant may separately adopt a retained parent
   revision/snapshot when its own exact-root watch is registered.
7. Return core cursor `(watch_id, 0)` with `fresh_instance=true`. Initialize
alone does not mint an fsmonitor clock. After the namespace daemon binds the
exact root/grant and successfully arms both the mandatory root-path-binding and
mount-topology monitors, the first ordinary query takes a later cut and
establishes a clock. Sequence 0 never names an fsmonitor boundary or clock.

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
   change, and update the revision's ownership/security summary from old/new
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
must prove they are ordered cuts of the same watch branch. If revision B
already exists, reuse it and
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

### 8.3 Garbage collection (planned)

Pins are owned by watch physical heads, pending cuts, active comparisons, and
revocable `retention_leases`. Creating a caller retention lease requires
`RETAIN` and inserts its pin atomically; release, expiry under a new fence, or
grant revocation removes that pin atomically.

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
retained comparison, or descendant delta depends on it; checkpoint
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
for pins and active operations. The SQLite `snapshot_pins_only_present`
trigger provides a second guard. Pending
cuts pin their base when the operation is reserved and their target in the same
transaction which registers the new snapshot.

After a full checkpoint is ready, storage compaction may atomically clear that
revision's `storage_base_revision_id`, set its depth to zero, and remove its
now-redundant overrides. This severs the physical dependency while immutable
comparison provenance remains. Deleting a watch rotates and disables its clock,
revokes its grants, and clears its heads and pins. Tombstones therefore do not
pin logical revisions forever, and the partial live-watch uniqueness index
permits the same still-existing writable subvolume to be initialized again.

Because `revisions.provenance_comparison_id` is a foreign key, the lightweight
comparison header/tombstone remains while that revision exists. Retention may
delete its bulky object/ref/event payload after it is no longer replayable, but
not the provenance row itself.

### 8.4 Abort and retry

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
Topology leases and watch cut leases are released in the same fenced abort or
left to expire for takeover. Startup performs these transitions for abandoned
operations before admitting a conflicting Initialize, cut, or GC.

## 9. Crash recovery and idempotency

Filesystem mutations and SQLite commits cannot be atomic. Every mutating API
accepts or creates an idempotency key and records an operation intent before
the Btrfs action:

```text
Initialize/Cut:
  planned -> fs_started -> fs_created -> uuid_recorded -> manifest_ready
          -> index_committed -> done

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
  snapshot creation/deletion. No conflicting retry,
  fence transfer, publication, deletion, or caller response is allowed while a
  receipt is nonterminal or its outcome is ambiguous;
- intent exists, deterministic path absent: retry or fail the operation;
- protected managed path exists while its operation is not finished: inspect
  it; if a target UUID was already recorded, require it, and otherwise adopt
  only when the intent's source/parent UUID, FSID, flags, path, and immutable
  requested operation match, then record the kernel-generated UUID. The object
  need not encode the creator's expired fence; the takeover's current fence is
  required for publication. Quarantine/report every identity mismatch;
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
  operation/connection state and reclaims expired query leases/pins under new
  fences.

The startup implementation recognizes only exact `manifest-...part`,
`full-index-...`, and `target-index-...` private spool formats and verifies
owner, mode, link count, and regular-file type before unlinking. After intended
operations have been reconciled, exact manager-looking `cut-*` and
`cut-initialize-*` names which are not claimed by a durable
snapshot or nonterminal operation are renamed into the manager's mode-0700
`quarantine/` directory. They are never adopted or deleted. A path recorded as
`deleted` or `lost` which reappears is a durability fault and blocks startup.

Fencing tokens, not lease time alone, prevent a paused worker from publishing
after recovery assigned its operation to another worker.

## 10. Direct immutable-snapshot client boundary

The supported client boundary is the private direct-scan API. A client asks the
per-user namespace daemon for one immutable read-only snapshot and receives an
opaque authenticated cursor, conservative invalidation, a bounded lease, and an
open directory descriptor for that exact snapshot. The client reads through the
descriptor rather than through a mutable live checkout.

Discovery is intentionally small:

```text
btrfs-awacs scan-sockname <absolute-live-root>
```

The command starts or reuses the daemon scoped to the caller's mount namespace
and prints one NUL-terminated absolute path to `scan.sock`. An explicit socket
override may bypass discovery, but it does not bypass peer-credential,
mount-namespace, process-root, grant, or snapshot-identity checks.

The private `SOCK_SEQPACKET` protocol has three lifecycle operations:

| Operation | Result | Correctness boundary |
| --- | --- | --- |
| `Begin(live_root, previous_cursor?)` | session ID, new cursor, invalidation, deadline, snapshot identity, one directory fd | Canonicalize and authorize the exact root, publish one cut, pin its immutable inputs, and transfer only the verified read-only snapshot fd. |
| `Renew(session_id)` | extended deadline | Extend the durable query lease while the client still owns the scan root. |
| `Finish(session_id, Committed|Aborted)` | acknowledgment | Release the pinned response only after the caller has durably accepted or rejected the cursor. |

A missing, foreign, expired, or unverifiable previous cursor produces a full
invalidation against the newly selected immutable snapshot. Exact paths are
returned only when retained comparison history proves the interval; subtree
moves, continuity loss, guard gaps, or missing history conservatively become a
full scan. The cursor is bound to store UUID, watch/grant, namespace-monitor
session, epoch, cut sequence, algorithm version, and target snapshot UUID.

The daemon lazily registers each requested canonical root. It first reuses an
active grant at that exact path, then tries to adopt a retained Btrfs snapshot
descendant, and otherwise initializes a new watch. One root never authorizes an
ancestor or sibling. The optional recursive precision journal can narrow
invalidations, but its absence or failure cannot weaken snapshot correctness.

This direct API does not expose a general filesystem-monitor protocol, command
execution, subscriptions, hooks, legacy compatibility framing, or mutable-checkout
incremental promises. The supported client must validate the received
filesystem/subvolume identity, scan only the leased descriptor, renew before
expiry, and persist the cursor only with state derived from that same snapshot.

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
   fd passing, durable watch grants, the root-owned execution-receipt journal
   and revocation/dispatch gate, best-effort current limits, and fixed
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
   owner/privileged-metadata security summary, persist events, and CAS
   the head. Property-test
   `apply(full(A), delta(A,B)) == full(B)`.
7. **Implement concurrent Changes.** Add per-watch cut leases, comparison-job
   deduplication, ordered publication, deterministic retry/replay, direct
   same-branch historical A -> B comparisons, and the full-checkpoint
   gap/fresh-instance transaction.
8. **Prove the dirty-witness foundation before exposing cursors.** Make every
   inode-item change, especially directory witnesses, survive normalization and
   projection. Add the snapshot-only post-cursor race/xfstest matrix for every
   supported mutation mechanism and refuse facade activation on a kernel/ABI
   which fails it. Implement exact aliases for file changes and conservative
   full invalidation for every changed directory.
9. **Add namespace continuity, cursors, and scan transactions.** Build the
   per-user daemon with peer credentials, mandatory top-down
   root-path-binding watches, the persistent mountinfo monitor, component/UUID
   re-resolution, and epoch rotation. Then implement authenticated
   watch/grant/view-scoped cursors, writer-serialized cut admission/coalescing,
   the cut replay floor, renewable query leases/pins, adjacent-cut witness
   aggregation, conservative invalidations, final response fencing, and
   byte-oriented limits. Property-test that every result is safe for the exact
   immutable snapshot descriptor paired with its cursor.
10. **Add the optional precision journal.** Implement recursive inotify on a
    separate fd, top-down arming, post-snapshot private markers, durable cursor
    assignment, exact create/delete/rename names, scoped directory-create and
    move-in prefixes, guard retention/floors, and coarse fallback on every gap.
    Benchmark how often this converts full scans into exact incremental
    matches; correctness must continue to pass with it disabled.
11. **Add the direct scan endpoint.** Implement bounded seqpacket framing,
    namespace-specific `scan-sockname` discovery, peer namespace/grant checks,
    exact snapshot descriptor transfer, renewal and completion fencing, and
    byte-exact fixtures for Begin/Renew/Finish. Patch the client to scan only
    the leased descriptor, validate its identity, and persist a cursor only
    with state derived from that same snapshot.
13. **Implement GC/recovery.** Add physical pins, two-phase snapshot deletion,
   filesystem commit barriers, revision/event retention, checkpoint compaction,
   independent cut/precision-floor advancement, query/admission lease
   reclamation, retention expiry/revocation, grant-generation reauthorization,
   root-owned broker-receipt reconciliation before fence takeover,
   view-monitor rules, orphan reconciliation, and fault injection at every
   filesystem/SQLite boundary.
14. **Stabilize the kernel ABI.** Document v2 structs, add fd-anchored roots,
   stream identities/footer, inode-only change masks including
   `BTRFS_CHANGED_OBJECT_CHANGE_FILE_DATA` and
   `BTRFS_CHANGED_OBJECT_CHANGE_DIR_ENTRIES`, an explicit monotonic dirty
   sequence, and nested-boundary
   semantics; keep full-index traversal in userspace;
   add the optional per-subvolume precise mutation-journal
   ABI, Btrfs/xfstests, and parser fuzzing. Keep the broker even if a later
   kernel authorization model permits selected unprivileged watches; replace
   inotify only after the kernel journal passes the same post-clock race suite.
15. **Validate performance and correctness.** Benchmark initialization,
    snapshot latency, kernel comparison, SQLite application, precision ingestion,
    path/alias expansion, direct invalidation projection, and GC
    separately. Test direct A -> C final state against A -> B -> C and test
    post-cursor transient activity with precision disabled and enabled.
    Exercise simultaneous callers, killed workers, disk-full SQLite, truncated
    manifests, snapshot deletion races, large hardlink sets, cursor expiration,
    namespace/grant changes, and every full-invalidation path.

## 12. Current prototype mapping

The benchmark `snap` and `compare` commands remain available. Source inspection
establishes the following current implementation boundaries:

- The core contains Btrfs filesystem/subvolume inspection, read-only snapshot
  creation, a privileged broker boundary, persistent SQLite watches/grants,
  immutable revision checkpoints and overlays, changed-object parsing, and
  indexed Initialize/Changes paths.
- Watch registration can adopt an already-existing Btrfs snapshot descendant
  whose exact parent identity resolves to a retained indexed revision. This
  reuses existing index rows but does not create or manage the descendant and
  never creates a sequence-0 direct-scan cursor.
- A private direct-scan endpoint provides `scan-sockname`, Begin, Renew, and
  Finish over a namespace-scoped `scan.sock`.
- The code and schema include clock boundaries, cut admissions, query leases,
  retention/delete intents, broker receipts, recovery, namespace continuity,
  and optional precision-journal scaffolding. Their presence does not prove
  that every associated production path is complete or correct.

The following production-critical properties are unresolved and must not be
represented as established behavior:

- Complete changed-object stream header endpoint-identity and completion-count
  validation before index mutation.
- Exact-baseline historical replay, fresh/full-invalidation propagation, and
  conservative projection of directory dirty witnesses; optional precision
  events are not reliably incorporated into the client-visible result.
- Terminal cut-failure handling and restart recovery that preserve physical
  snapshot heads and refuse unsafe incremental continuation.
- Working physical/logical garbage collection, snapshot/history retention,
  replay-floor advancement, and the expiry behavior associated with those
  policies.
- Proven concurrent query/cut coalescing, lock behavior, and daemon discovery
  or automatic activation across the actual installed client entry points.
- A checked-in runnable modified-kernel/UML end-to-end harness exercising
  real direct-scan clients, their wire protocol, failure cases, and adversarial
  mutation timing. Existing proposed acceptance matrices are tests to run, not
  passing test results.

[FIXES.md](../FIXES.md) records the known correctness and performance findings.
Until those issues are resolved and the real-client matrix passes, the service
must not be described as a verified production direct-scan backend.

## References

- [Btrfs subvolume documentation](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html)
- [Btrfs ioctl documentation](https://btrfs.readthedocs.io/en/stable/btrfs-ioctl.html)
- [Btrfs mount options (`user_subvol_rm_allowed`)](https://btrfs.readthedocs.io/en/latest/ch-mount-options.html)
- [SQLite write-ahead logging](https://sqlite.org/wal.html)
