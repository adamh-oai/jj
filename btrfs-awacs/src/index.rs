use crate::manifest::{CHANGE_INODE, CHANGE_REF, ChangedObjectsManifest, ObjectChange};
use std::collections::{BTreeMap, BTreeSet};

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
    base_paths: &BTreeMap<u64, Vec<Vec<u8>>>,
    target_paths: &BTreeMap<u64, Vec<Vec<u8>>>,
) -> Vec<Event> {
    let mut events = Vec::new();
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
            events.push(event(
                change,
                EventKind::SubtreeMoved,
                old_set.first().cloned(),
                new_set.first().cloned(),
            ));
        } else {
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
    events.sort_by(|left, right| event_key(left).cmp(&event_key(right)));
    events.dedup();
    events
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
    use crate::manifest::{CHANGE_FILE_DATA, Reference};

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
    fn broker_endpoint_paths_emit_subtree_move() {
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
            &base_paths,
            &target_paths,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::SubtreeMoved);
        assert_eq!(events[0].old_path.as_deref(), Some(b"old".as_slice()));
        assert_eq!(events[0].new_path.as_deref(), Some(b"new".as_slice()));
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
        let events = derive_events_from_endpoint_paths(&manifest, &BTreeMap::new(), &paths, &paths);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].new_path.as_deref(), Some(b"a".as_slice()));
        assert_eq!(events[1].new_path.as_deref(), Some(b"b".as_slice()));
    }
}
