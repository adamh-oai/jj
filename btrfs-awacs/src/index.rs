use crate::manifest::{CHANGE_INODE, CHANGE_REF, ChangedObjectsManifest, ObjectChange};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub const MODE_TYPE_MASK: u32 = 0o170000;
pub const MODE_DIRECTORY: u32 = 0o040000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Object {
    pub ino: u64,
    pub generation: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u64,
    pub gid: u64,
    pub rdev: u64,
    pub privilege_flags: u64,
    pub security_xattr_hash: [u8; 32],
}

impl Object {
    pub fn is_directory(&self) -> bool {
        self.mode & MODE_TYPE_MASK == MODE_DIRECTORY
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventKind {
    PathAdded,
    PathRemoved,
    PathChanged,
    SubtreeMoved,
    DirectoryDirtyWitness,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub kind: EventKind,
    pub ino: u64,
    pub old_generation: Option<u64>,
    pub new_generation: Option<u64>,
    pub change_mask: u64,
    pub old_path: Option<Vec<u8>>,
    pub new_path: Option<Vec<u8>>,
}

/// Derives canonical replay events from exact paths resolved against the two
/// immutable comparison endpoints.
pub fn derive_events_from_endpoint_paths(
    manifest: &ChangedObjectsManifest,
    target_objects: &BTreeMap<u64, Object>,
    (base, base_paths): (BorrowedFd<'_>, &BTreeMap<u64, Vec<Vec<u8>>>),
    (target, target_paths): (BorrowedFd<'_>, &BTreeMap<u64, Vec<Vec<u8>>>),
) -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    let mut moves = Vec::new();
    for change in manifest.objects.values().copied() {
        let old_set: BTreeSet<_> = base_paths
            .get(&change.ino)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let new_set: BTreeSet<_> = target_paths
            .get(&change.ino)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();

        if change.is_deleted() && !change.is_created() {
            for path in old_set {
                events.push(event(change, EventKind::PathRemoved, Some(path), None));
            }
            continue;
        }
        if change.is_created() && !change.is_deleted() {
            for path in new_set {
                events.push(event(change, EventKind::PathAdded, None, Some(path)));
            }
            continue;
        }

        let is_directory = target_objects
            .get(&change.ino)
            .is_some_and(Object::is_directory);
        if is_directory
            && change.change_mask & CHANGE_REF != 0
            && old_set.len() == 1
            && new_set.len() == 1
            && old_set != new_set
        {
            moves.push(event(
                change,
                EventKind::SubtreeMoved,
                old_set.first().cloned(),
                new_set.first().cloned(),
            ));
        }
        for path in old_set.difference(&new_set) {
            events.push(event(
                change,
                EventKind::PathRemoved,
                Some(path.clone()),
                None,
            ));
        }
        for path in new_set.difference(&old_set) {
            events.push(event(
                change,
                EventKind::PathAdded,
                None,
                Some(path.clone()),
            ));
        }

        if change.change_mask & !CHANGE_REF != 0 {
            for path in old_set.intersection(&new_set) {
                events.push(event(
                    change,
                    EventKind::PathChanged,
                    Some(path.clone()),
                    Some(path.clone()),
                ));
            }
        }
        if is_directory && change.change_mask & CHANGE_INODE != 0 {
            for path in &new_set {
                events.push(event(
                    change,
                    EventKind::DirectoryDirtyWitness,
                    None,
                    Some(path.clone()),
                ));
            }
        }
    }
    for (snapshot, kind) in [
        (base, EventKind::PathRemoved),
        (target, EventKind::PathAdded),
    ] {
        append_moved_descendants(&mut events, &moves, manifest, snapshot, kind)?;
    }
    events.sort_by(|left, right| event_key(left).cmp(&event_key(right)));
    events.dedup();
    Ok(events)
}

fn append_moved_descendants(
    events: &mut Vec<Event>,
    moves: &[Event],
    manifest: &ChangedObjectsManifest,
    snapshot: BorrowedFd<'_>,
    kind: EventKind,
) -> io::Result<()> {
    let removed = kind == EventKind::PathRemoved;
    let roots: BTreeMap<_, _> = moves
        .iter()
        .filter_map(|moved| {
            let name = if removed {
                &moved.old_path
            } else {
                &moved.new_path
            };
            Some((Path::new(OsStr::from_bytes(name.as_deref()?)), moved))
        })
        .collect();
    for (&root, moved) in &roots {
        // Nested moves can overlap on just one side of the comparison.
        if root.ancestors().skip(1).any(|p| roots.contains_key(p)) {
            continue;
        }
        let start = events.len();
        let result = crate::btrfs::visit_snapshot_descendants(snapshot, root, |path, ino| {
            // An ancestor rename changes the path of an otherwise unchanged inode.
            let change = manifest.objects.get(&ino).copied().unwrap_or(ObjectChange {
                ino,
                old_generation: None,
                new_generation: None,
                change_mask: 0,
            });
            let path = path.as_os_str().as_bytes().to_vec();
            let (old, new) = if removed {
                (Some(path), None)
            } else {
                (None, Some(path))
            };
            events.push(event(change, kind.clone(), old, new));
        });
        // Other consumers can rescan a prefix even if the manager lacks
        // read permission. Never publish a partial list as an exact move.
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::PermissionDenied {
                return Err(error);
            }
            events.truncate(start);
            events.push((*moved).clone());
        }
    }
    Ok(())
}

fn event(
    change: ObjectChange,
    kind: EventKind,
    old_path: Option<Vec<u8>>,
    new_path: Option<Vec<u8>>,
) -> Event {
    Event {
        kind,
        ino: change.ino,
        old_generation: change.old_generation,
        new_generation: change.new_generation,
        change_mask: change.change_mask,
        old_path,
        new_path,
    }
}

fn event_key(event: &Event) -> (u8, &[u8], &[u8], u64) {
    let kind = match event.kind {
        EventKind::PathRemoved => 0,
        EventKind::PathAdded => 1,
        EventKind::PathChanged => 2,
        EventKind::SubtreeMoved => 3,
        EventKind::DirectoryDirtyWitness => 4,
    };
    (
        kind,
        event.old_path.as_deref().unwrap_or_default(),
        event.new_path.as_deref().unwrap_or_default(),
        event.ino,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::project_events;
    use crate::manifest::{CHANGE_FILE_DATA, Reference};
    use std::fs::{self, File};
    use std::os::fd::AsFd;
    use std::os::unix::fs::symlink;

    fn object(ino: u64, mode: u32) -> Object {
        Object {
            ino,
            generation: ino + 100,
            mode,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            privilege_flags: 0,
            security_xattr_hash: [0; 32],
        }
    }

    #[test]
    fn moved_directory_records_exact_children_from_each_endpoint() {
        let base = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::create_dir_all(base.path().join("old/nested")).unwrap();
        fs::write(base.path().join("old/nested/removed"), []).unwrap();
        fs::create_dir_all(target.path().join("new/nested/empty")).unwrap();
        fs::write(target.path().join("new/nested/added"), []).unwrap();
        fs::hard_link(
            target.path().join("new/nested/added"),
            target.path().join("new/link"),
        )
        .unwrap();
        symlink("../..", base.path().join("old/nested/cycle")).unwrap();
        fs::create_dir(base.path().join("unrelated")).unwrap();
        let raw = OsStr::from_bytes(b"old/nested/\xff");
        fs::write(base.path().join(raw), []).unwrap();
        let change = ObjectChange {
            ino: 300,
            old_generation: Some(400),
            new_generation: Some(400),
            change_mask: CHANGE_REF,
        };
        let manifest = ChangedObjectsManifest {
            objects: [(300, change)].into(),
            ref_adds: [Reference {
                ino: 300,
                parent_ino: 256,
                name: b"new".to_vec(),
            }]
            .into(),
            ref_deletes: [Reference {
                ino: 300,
                parent_ino: 256,
                name: b"old".to_vec(),
            }]
            .into(),
            raw_ref_adds: 1,
            raw_ref_deletes: 1,
        };
        let target_objects = BTreeMap::from([(300, object(300, MODE_DIRECTORY | 0o755))]);
        let base_paths = BTreeMap::from([(300, vec![b"old".to_vec()])]);
        let target_paths = BTreeMap::from([(300, vec![b"new".to_vec()])]);
        let events = derive_events_from_endpoint_paths(
            &manifest,
            &target_objects,
            (File::open(base.path()).unwrap().as_fd(), &base_paths),
            (File::open(target.path()).unwrap().as_fd(), &target_paths),
        )
        .unwrap();
        drop((base, target));
        assert!(events.iter().all(|event| match event.kind {
            EventKind::PathRemoved => event.old_path.is_some() && event.new_path.is_none(),
            EventKind::PathAdded => event.new_path.is_some() && event.old_path.is_none(),
            _ => false,
        }));
        let projection = project_events(&events);
        assert!(!projection.fresh_instance);
        assert!(projection.prefixes.is_empty());
        assert_eq!(
            projection.paths,
            [
                b"new".as_slice(),
                b"new/link",
                b"new/nested",
                b"new/nested/added",
                b"new/nested/empty",
                b"old",
                b"old/nested",
                b"old/nested/cycle",
                b"old/nested/removed",
                raw.as_bytes(),
            ]
        );
    }

    #[test]
    fn broker_endpoint_paths_emit_every_hardlink_change() {
        let change = ObjectChange {
            ino: 300,
            old_generation: Some(400),
            new_generation: Some(400),
            change_mask: CHANGE_FILE_DATA,
        };
        let manifest = ChangedObjectsManifest {
            objects: [(300, change)].into(),
            ref_adds: BTreeSet::new(),
            ref_deletes: BTreeSet::new(),
            raw_ref_adds: 0,
            raw_ref_deletes: 0,
        };
        let paths = BTreeMap::from([(300, vec![b"a".to_vec(), b"b".to_vec()])]);
        let dir = tempfile::tempdir().unwrap();
        let root = File::open(dir.path()).unwrap();
        let events = derive_events_from_endpoint_paths(
            &manifest,
            &BTreeMap::new(),
            (root.as_fd(), &paths),
            (root.as_fd(), &paths),
        )
        .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].new_path.as_deref(), Some(b"a".as_slice()));
        assert_eq!(events[1].new_path.as_deref(), Some(b"b".as_slice()));
    }
}
