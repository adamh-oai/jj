use crate::manifest::{
    ChangedObjectsManifest, ObjectChange, Reference, CHANGE_INODE, CHANGE_REF, CHANGE_XATTR,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const ROOT_INO: u64 = 256;
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Index {
    pub objects: BTreeMap<u64, Object>,
    pub references: BTreeSet<Reference>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafetySummary {
    pub single_owner_uid: Option<u64>,
    pub privileged_metadata_count: u64,
    pub security_state_hash: [u8; 32],
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyResult {
    pub index: Index,
    pub events: Vec<Event>,
    pub safety: SafetySummary,
    pub state_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexError {
    message: String,
}

impl IndexError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IndexError {}

impl Index {
    pub fn validate(&self) -> Result<(), IndexError> {
        let root = self
            .objects
            .get(&ROOT_INO)
            .ok_or_else(|| IndexError::new("index has no root inode 256"))?;
        if !root.is_directory() {
            return Err(IndexError::new("root inode 256 is not a directory"));
        }

        for (&ino, object) in &self.objects {
            if ino == 0 || object.ino != ino {
                return Err(IndexError::new(format!(
                    "object map key {ino} does not match inode {}",
                    object.ino
                )));
            }
            if object.generation == 0 || object.nlink == 0 {
                return Err(IndexError::new(format!(
                    "inode {ino} has invalid generation or link count"
                )));
            }
            if object.mode & MODE_TYPE_MASK == 0 {
                return Err(IndexError::new(format!(
                    "inode {ino} has no file type in mode"
                )));
            }
        }

        let mut refs_by_child: BTreeMap<u64, Vec<&Reference>> = BTreeMap::new();
        let mut names = BTreeSet::new();
        for reference in &self.references {
            if reference.ino == ROOT_INO {
                return Err(IndexError::new("root inode has a parent reference"));
            }
            if reference.ino == 0
                || reference.parent_ino == 0
                || reference.name.is_empty()
                || reference.name.contains(&b'/')
                || reference.name.contains(&b'\0')
            {
                return Err(IndexError::new("index contains an invalid reference"));
            }
            if !self.objects.contains_key(&reference.ino) {
                return Err(IndexError::new(format!(
                    "reference names absent child inode {}",
                    reference.ino
                )));
            }
            let parent = self.objects.get(&reference.parent_ino).ok_or_else(|| {
                IndexError::new(format!(
                    "reference names absent parent inode {}",
                    reference.parent_ino
                ))
            })?;
            if !parent.is_directory() {
                return Err(IndexError::new(format!(
                    "reference parent inode {} is not a directory",
                    reference.parent_ino
                )));
            }
            if !names.insert((reference.parent_ino, reference.name.clone())) {
                return Err(IndexError::new(format!(
                    "more than one child has parent {} and name {:?}",
                    reference.parent_ino, reference.name
                )));
            }
            refs_by_child
                .entry(reference.ino)
                .or_default()
                .push(reference);
        }

        for (&ino, object) in &self.objects {
            if ino == ROOT_INO {
                continue;
            }
            let count = refs_by_child.get(&ino).map_or(0, Vec::len);
            if count == 0 {
                return Err(IndexError::new(format!(
                    "non-root inode {ino} is unreachable"
                )));
            }
            if object.is_directory() && count != 1 {
                return Err(IndexError::new(format!(
                    "directory inode {ino} has {count} parent references"
                )));
            }
        }

        let mut memo = BTreeMap::new();
        for &ino in self.objects.keys() {
            self.paths_inner(ino, &refs_by_child, &mut memo, &mut BTreeSet::new())?;
        }
        Ok(())
    }

    pub fn paths(&self, ino: u64) -> Result<Vec<Vec<u8>>, IndexError> {
        if !self.objects.contains_key(&ino) {
            return Err(IndexError::new(format!("inode {ino} is absent")));
        }
        let mut refs_by_child: BTreeMap<u64, Vec<&Reference>> = BTreeMap::new();
        for reference in &self.references {
            refs_by_child
                .entry(reference.ino)
                .or_default()
                .push(reference);
        }
        self.paths_inner(
            ino,
            &refs_by_child,
            &mut BTreeMap::new(),
            &mut BTreeSet::new(),
        )
    }

    fn paths_inner(
        &self,
        ino: u64,
        refs_by_child: &BTreeMap<u64, Vec<&Reference>>,
        memo: &mut BTreeMap<u64, Vec<Vec<u8>>>,
        visiting: &mut BTreeSet<u64>,
    ) -> Result<Vec<Vec<u8>>, IndexError> {
        if let Some(paths) = memo.get(&ino) {
            return Ok(paths.clone());
        }
        if ino == ROOT_INO {
            return Ok(vec![Vec::new()]);
        }
        if !visiting.insert(ino) {
            return Err(IndexError::new(format!(
                "directory ancestry contains a cycle through inode {ino}"
            )));
        }
        let refs = refs_by_child
            .get(&ino)
            .ok_or_else(|| IndexError::new(format!("inode {ino} has no parent reference")))?;
        let mut paths = Vec::new();
        for reference in refs {
            if !self.objects.contains_key(&reference.parent_ino) {
                return Err(IndexError::new(format!(
                    "inode {ino} has absent parent {}",
                    reference.parent_ino
                )));
            }
            for mut parent_path in
                self.paths_inner(reference.parent_ino, refs_by_child, memo, visiting)?
            {
                if !parent_path.is_empty() {
                    parent_path.push(b'/');
                }
                parent_path.extend_from_slice(&reference.name);
                paths.push(parent_path);
            }
        }
        visiting.remove(&ino);
        paths.sort();
        paths.dedup();
        memo.insert(ino, paths.clone());
        Ok(paths)
    }

    pub fn state_hash(&self) -> [u8; 32] {
        let mut state = [0_u8; 32];
        for object in self.objects.values() {
            xor_digest(&mut state, &object_state_digest(object));
        }
        for reference in &self.references {
            xor_digest(&mut state, &reference_state_digest(reference));
        }
        state
    }

    pub fn safety_summary(&self) -> SafetySummary {
        let mut owner = None;
        let mut mixed_owner = false;
        let mut privileged_metadata_count = 0_u64;
        let mut security_state_hash = [0_u8; 32];
        for object in self.objects.values() {
            match owner {
                None => owner = Some(object.uid),
                Some(uid) if uid != object.uid => mixed_owner = true,
                Some(_) => {}
            }
            if object.privilege_flags != 0 {
                privileged_metadata_count += 1;
            }
            xor_digest(&mut security_state_hash, &object_security_digest(object));
        }
        SafetySummary {
            single_owner_uid: (!mixed_owner).then_some(owner).flatten(),
            privileged_metadata_count,
            security_state_hash,
        }
    }
}

pub(crate) fn object_state_digest(object: &Object) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-index-object-v2\0");
    hash.update(object.ino.to_be_bytes());
    hash.update(object.generation.to_be_bytes());
    hash.update(object.mode.to_be_bytes());
    hash.update(object.nlink.to_be_bytes());
    hash.update(object.uid.to_be_bytes());
    hash.update(object.gid.to_be_bytes());
    hash.update(object.rdev.to_be_bytes());
    hash.update(object.privilege_flags.to_be_bytes());
    hash.update(object.security_xattr_hash);
    hash.finalize().into()
}

pub(crate) fn reference_state_digest(reference: &Reference) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-index-reference-v2\0");
    hash.update(reference.ino.to_be_bytes());
    hash.update(reference.parent_ino.to_be_bytes());
    hash.update((reference.name.len() as u64).to_be_bytes());
    hash.update(&reference.name);
    hash.finalize().into()
}

pub(crate) fn object_security_digest(object: &Object) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-security-object-v2\0");
    hash.update(object.ino.to_be_bytes());
    hash.update(object.uid.to_be_bytes());
    hash.update(object.gid.to_be_bytes());
    hash.update(object.mode.to_be_bytes());
    hash.update(object.rdev.to_be_bytes());
    hash.update(object.privilege_flags.to_be_bytes());
    hash.update(object.security_xattr_hash);
    hash.finalize().into()
}

pub(crate) fn xor_digest(state: &mut [u8; 32], digest: &[u8; 32]) {
    for (byte, digest_byte) in state.iter_mut().zip(digest) {
        *byte ^= digest_byte;
    }
}

pub fn apply_manifest(
    base: &Index,
    manifest: &ChangedObjectsManifest,
    target_objects: &BTreeMap<u64, Object>,
) -> Result<ApplyResult, IndexError> {
    base.validate()?;
    validate_delta_inputs(base, manifest, target_objects)?;

    let mut target = base.clone();
    for reference in &manifest.ref_deletes {
        if !target.references.remove(reference) {
            return Err(IndexError::new(format!(
                "delta deletes absent reference ({}, {}, {:?})",
                reference.ino, reference.parent_ino, reference.name
            )));
        }
    }
    for change in manifest.objects.values() {
        if change.is_deleted() && !change.is_created() {
            target.objects.remove(&change.ino);
        } else if let Some(object) = target_objects.get(&change.ino) {
            target.objects.insert(change.ino, object.clone());
        }
    }
    for reference in &manifest.ref_adds {
        if !target.references.insert(reference.clone()) {
            return Err(IndexError::new(format!(
                "delta adds existing reference ({}, {}, {:?})",
                reference.ino, reference.parent_ino, reference.name
            )));
        }
    }
    target.validate()?;
    let events = derive_events(base, &target, manifest)?;
    Ok(ApplyResult {
        state_hash: target.state_hash(),
        safety: target.safety_summary(),
        index: target,
        events,
    })
}

fn validate_delta_inputs(
    base: &Index,
    manifest: &ChangedObjectsManifest,
    target_objects: &BTreeMap<u64, Object>,
) -> Result<(), IndexError> {
    for (&ino, object) in target_objects {
        if object.ino != ino {
            return Err(IndexError::new(format!(
                "target object key {ino} does not match inode {}",
                object.ino
            )));
        }
        let Some(change) = manifest.objects.get(&ino) else {
            return Err(IndexError::new(format!(
                "target attributes supplied for unchanged inode {ino}"
            )));
        };
        if change.is_deleted() && !change.is_created() {
            return Err(IndexError::new(format!(
                "target attributes supplied for deleted inode {ino}"
            )));
        }
    }

    for change in manifest.objects.values() {
        let old = base.objects.get(&change.ino);
        if change.is_created() && !change.is_deleted() {
            if old.is_some() {
                return Err(IndexError::new(format!(
                    "created inode {} already exists in the base",
                    change.ino
                )));
            }
        } else {
            let old = old.ok_or_else(|| {
                IndexError::new(format!(
                    "changed inode {} is absent from the base",
                    change.ino
                ))
            })?;
            if change
                .old_generation
                .is_some_and(|generation| generation != old.generation)
            {
                return Err(IndexError::new(format!(
                    "inode {} old generation does not match the base",
                    change.ino
                )));
            }
        }

        let requires_target = change.is_created()
            || (!change.is_deleted() && change.change_mask & (CHANGE_INODE | CHANGE_XATTR) != 0);
        if requires_target {
            let object = target_objects.get(&change.ino).ok_or_else(|| {
                IndexError::new(format!(
                    "changed inode {} requires authoritative target attributes",
                    change.ino
                ))
            })?;
            if change.new_generation != Some(object.generation) {
                return Err(IndexError::new(format!(
                    "inode {} target generation does not match the manifest",
                    change.ino
                )));
            }
        } else if let Some(new_generation) = change.new_generation {
            let old = old.expect("non-created change checked above");
            if new_generation != old.generation {
                return Err(IndexError::new(format!(
                    "inode {} changed generation without target attributes",
                    change.ino
                )));
            }
        }
    }
    Ok(())
}

fn derive_events(
    base: &Index,
    target: &Index,
    manifest: &ChangedObjectsManifest,
) -> Result<Vec<Event>, IndexError> {
    let mut events = Vec::new();
    for change in manifest.objects.values().copied() {
        let old_paths = if base.objects.contains_key(&change.ino) {
            base.paths(change.ino)?
        } else {
            Vec::new()
        };
        let new_paths = if target.objects.contains_key(&change.ino) {
            target.paths(change.ino)?
        } else {
            Vec::new()
        };
        let old_set: BTreeSet<_> = old_paths.into_iter().collect();
        let new_set: BTreeSet<_> = new_paths.into_iter().collect();

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

        let is_directory = target
            .objects
            .get(&change.ino)
            .or_else(|| base.objects.get(&change.ino))
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

        let has_object_change = change.change_mask & !CHANGE_REF != 0;
        if has_object_change {
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
    Ok(events)
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
    use crate::manifest::{CHANGE_FILE_DATA, CHANGE_REF};

    fn object(ino: u64, mode: u32, nlink: u32) -> Object {
        Object {
            ino,
            generation: ino + 100,
            mode,
            nlink,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            privilege_flags: 0,
            security_xattr_hash: [0; 32],
        }
    }

    fn base_index() -> Index {
        let mut index = Index::default();
        index
            .objects
            .insert(ROOT_INO, object(ROOT_INO, MODE_DIRECTORY | 0o755, 1));
        index
    }

    #[test]
    fn resolves_every_hardlink_path_as_raw_bytes() {
        let mut index = base_index();
        index.objects.insert(300, object(300, 0o100644, 2));
        index.references.insert(Reference {
            ino: 300,
            parent_ino: ROOT_INO,
            name: b"a".to_vec(),
        });
        index.references.insert(Reference {
            ino: 300,
            parent_ino: ROOT_INO,
            name: vec![0xff, b'b'],
        });
        index.validate().unwrap();
        assert_eq!(
            index.paths(300).unwrap(),
            vec![b"a".to_vec(), vec![0xff, b'b']]
        );
    }

    #[test]
    fn rejects_directory_cycles_and_duplicate_names() {
        let mut index = base_index();
        index
            .objects
            .insert(300, object(300, MODE_DIRECTORY | 0o755, 1));
        index
            .objects
            .insert(301, object(301, MODE_DIRECTORY | 0o755, 1));
        index.references.insert(Reference {
            ino: 300,
            parent_ino: 301,
            name: b"a".to_vec(),
        });
        index.references.insert(Reference {
            ino: 301,
            parent_ino: 300,
            name: b"b".to_vec(),
        });
        assert!(index.validate().unwrap_err().to_string().contains("cycle"));

        let mut index = base_index();
        index.objects.insert(300, object(300, 0o100644, 1));
        index.objects.insert(301, object(301, 0o100644, 1));
        for ino in [300, 301] {
            index.references.insert(Reference {
                ino,
                parent_ino: ROOT_INO,
                name: b"same".to_vec(),
            });
        }
        assert!(index
            .validate()
            .unwrap_err()
            .to_string()
            .contains("more than one child"));
    }

    #[test]
    fn applies_hardlink_delta_and_emits_all_aliases_for_content_change() {
        let mut base = base_index();
        base.objects.insert(300, object(300, 0o100644, 2));
        for name in [b"a".as_slice(), b"b".as_slice()] {
            base.references.insert(Reference {
                ino: 300,
                parent_ino: ROOT_INO,
                name: name.to_vec(),
            });
        }
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
        let result = apply_manifest(&base, &manifest, &BTreeMap::new()).unwrap();
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].new_path.as_deref(), Some(b"a".as_slice()));
        assert_eq!(result.events[1].new_path.as_deref(), Some(b"b".as_slice()));
    }

    #[test]
    fn emits_compact_subtree_move() {
        let mut base = base_index();
        base.objects
            .insert(300, object(300, MODE_DIRECTORY | 0o755, 1));
        base.references.insert(Reference {
            ino: 300,
            parent_ino: ROOT_INO,
            name: b"old".to_vec(),
        });
        let old_ref = base.references.iter().next().unwrap().clone();
        let new_ref = Reference {
            name: b"new".to_vec(),
            ..old_ref.clone()
        };
        let change = ObjectChange {
            ino: 300,
            old_generation: Some(400),
            new_generation: Some(400),
            change_mask: CHANGE_REF,
        };
        let manifest = ChangedObjectsManifest {
            objects: [(300, change)].into(),
            ref_adds: [new_ref].into(),
            ref_deletes: [old_ref].into(),
            raw_ref_adds: 1,
            raw_ref_deletes: 1,
        };
        let result = apply_manifest(&base, &manifest, &BTreeMap::new()).unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].kind, EventKind::SubtreeMoved);
        assert_eq!(
            result.events[0].old_path.as_deref(),
            Some(b"old".as_slice())
        );
        assert_eq!(
            result.events[0].new_path.as_deref(),
            Some(b"new".as_slice())
        );
    }

    #[test]
    fn refuses_inode_metadata_change_without_authoritative_target() {
        let base = base_index();
        let manifest = ChangedObjectsManifest {
            objects: [(
                ROOT_INO,
                ObjectChange {
                    ino: ROOT_INO,
                    old_generation: Some(356),
                    new_generation: Some(356),
                    change_mask: CHANGE_INODE,
                },
            )]
            .into(),
            ref_adds: BTreeSet::new(),
            ref_deletes: BTreeSet::new(),
            raw_ref_adds: 0,
            raw_ref_deletes: 0,
        };
        assert!(apply_manifest(&base, &manifest, &BTreeMap::new())
            .unwrap_err()
            .to_string()
            .contains("authoritative target attributes"));
    }

    #[test]
    fn state_hash_is_independent_of_insertion_order() {
        let mut left = base_index();
        left.objects.insert(300, object(300, 0o100644, 1));
        left.references.insert(Reference {
            ino: 300,
            parent_ino: ROOT_INO,
            name: b"x".to_vec(),
        });
        let mut right = Index::default();
        right.objects.insert(300, object(300, 0o100644, 1));
        right
            .objects
            .insert(ROOT_INO, object(ROOT_INO, MODE_DIRECTORY | 0o755, 1));
        right.references = left.references.clone();
        assert_eq!(left.state_hash(), right.state_hash());
    }
}
