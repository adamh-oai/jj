use std::os::fd::BorrowedFd;

use super::BTRFS_IOC_TREE_SEARCH_V2;
use super::BtrfsError;
use super::BtrfsIoctlSearchArgsV2;
use super::BtrfsIoctlSearchKey;
use super::ROOT_INODE;
use super::ioctl;

const INODE_REF_KEY: u32 = 12;
const INODE_EXTREF_KEY: u32 = 13;
const SEARCH_ITEM_HEADER_SIZE: usize = 32;
// A single metadata item fits in one Btrfs node, which is at most 64 KiB.
const SEARCH_BUFFER_SIZE: usize = 64 * 1024 + SEARCH_ITEM_HEADER_SIZE;

type RefKey = (u32, u64);

#[repr(C)]
struct RefSearchPage {
    args: BtrfsIoctlSearchArgsV2,
    bytes: [u8; SEARCH_BUFFER_SIZE],
}

impl RefSearchPage {
    fn new() -> Box<Self> {
        Box::new(Self {
            args: BtrfsIoctlSearchArgsV2 {
                key: BtrfsIoctlSearchKey::default(),
                buf_size: 0,
            },
            bytes: [0; SEARCH_BUFFER_SIZE],
        })
    }

    fn read(
        &mut self,
        fd: BorrowedFd<'_>,
        ino: u64,
        first: RefKey,
        last_kind: u32,
        limit: u32,
    ) -> Result<(u32, &[u8]), BtrfsError> {
        self.args = BtrfsIoctlSearchArgsV2 {
            key: BtrfsIoctlSearchKey {
                // Zero selects the tree containing the passed snapshot root.
                min_objectid: ino,
                max_objectid: ino,
                min_type: first.0,
                max_type: last_kind,
                min_offset: first.1,
                max_offset: u64::MAX,
                max_transid: u64::MAX,
                nr_items: limit,
                ..Default::default()
            },
            buf_size: SEARCH_BUFFER_SIZE as u64,
        };
        ioctl(
            fd,
            BTRFS_IOC_TREE_SEARCH_V2,
            self,
            "Btrfs inode reference search",
        )?;
        let count = self.args.key.nr_items;
        if count > limit || count as usize > self.bytes.len() / SEARCH_ITEM_HEADER_SIZE {
            return Err(BtrfsError::new("invalid Btrfs inode reference item count"));
        }
        Ok((count, &self.bytes))
    }
}

fn take_item<'a>(
    bytes: &mut &'a [u8],
    ino: u64,
    first: RefKey,
    last_kind: u32,
) -> Result<(RefKey, &'a [u8]), BtrfsError> {
    let (header, rest) = bytes
        .split_at_checked(SEARCH_ITEM_HEADER_SIZE)
        .ok_or_else(|| BtrfsError::new("truncated Btrfs tree-search header"))?;
    let object_id = u64::from_ne_bytes(header[8..16].try_into().unwrap());
    let offset = u64::from_ne_bytes(header[16..24].try_into().unwrap());
    let kind = u32::from_ne_bytes(header[24..28].try_into().unwrap());
    let length = u32::from_ne_bytes(header[28..32].try_into().unwrap()) as usize;
    let key = (kind, offset);
    if object_id != ino || key < first || kind > last_kind || length == 0 {
        return Err(BtrfsError::new("invalid Btrfs inode reference item"));
    }
    let (item, rest) = rest
        .split_at_checked(length)
        .ok_or_else(|| BtrfsError::new("truncated Btrfs tree-search item"))?;
    *bytes = rest;
    Ok((key, item))
}

fn take_reference<'a>(key: RefKey, bytes: &mut &'a [u8]) -> Result<(u64, &'a [u8]), BtrfsError> {
    // INODE_REF stores the parent in the key offset. INODE_EXTREF uses a
    // hash for its offset and stores the parent in each packed record.
    let parent = if key.0 == INODE_EXTREF_KEY {
        let (parent, rest) = bytes
            .split_at_checked(8)
            .ok_or_else(|| BtrfsError::new("truncated Btrfs extended inode reference"))?;
        *bytes = rest;
        u64::from_le_bytes(parent.try_into().unwrap())
    } else {
        key.1
    };
    let (header, rest) = bytes
        .split_at_checked(10)
        .ok_or_else(|| BtrfsError::new("truncated Btrfs inode reference"))?;
    let length = u16::from_le_bytes(header[8..10].try_into().unwrap()) as usize;
    let (name, rest) = rest
        .split_at_checked(length)
        .ok_or_else(|| BtrfsError::new("truncated Btrfs inode reference name"))?;
    if parent < ROOT_INODE
        || name.is_empty()
        || name.contains(&0)
        || name.contains(&b'/')
        || name == b"."
        || name == b".."
    {
        return Err(BtrfsError::new(
            "invalid Btrfs inode reference parent or name",
        ));
    }
    *bytes = rest;
    Ok((parent, name))
}

fn directory_path(
    page: &mut RefSearchPage,
    fd: BorrowedFd<'_>,
    mut directory: u64,
    max_path_bytes: usize,
    path: &mut Vec<u8>,
) -> Result<(), BtrfsError> {
    path.clear();
    let mut visited = std::collections::BTreeSet::new();
    while directory != ROOT_INODE {
        if !visited.insert(directory) {
            return Err(BtrfsError::new("cycle in Btrfs directory references"));
        }
        // A directory has one ordinary inode reference. Follow it inside the
        // snapshot tree without the fixed pathname limit of INO_LOOKUP.
        let first = (INODE_REF_KEY, 0);
        let (count, mut bytes) = page.read(fd, directory, first, INODE_REF_KEY, 2)?;
        if count != 1 {
            return Err(BtrfsError::new(
                "Btrfs directory must have one inode reference",
            ));
        }
        let (key, mut item) = take_item(&mut bytes, directory, first, INODE_REF_KEY)?;
        let (parent, name) = take_reference(key, &mut item)?;
        if !item.is_empty() {
            return Err(BtrfsError::new("Btrfs directory has multiple names"));
        }
        if name.len() + usize::from(!path.is_empty()) > max_path_bytes.saturating_sub(path.len()) {
            return Err(path_too_long());
        }
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend(name.iter().rev());
        directory = parent;
    }
    path.reverse();
    Ok(())
}

fn path_too_long() -> BtrfsError {
    BtrfsError::new("Btrfs inode path exceeds the remaining output limit")
}

/// Visits every name in the verified read-only snapshot after INO_PATHS
/// reports overflow. Emit paths one at a time so the broker can keep its
/// existing output limit without retaining all names of the inode in memory.
pub(crate) fn visit_inode_paths_from_tree<E: From<BtrfsError>>(
    fd: BorrowedFd<'_>,
    ino: u64,
    max_path_bytes: usize,
    mut visit: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let mut page = RefSearchPage::new();
    let mut directory_page = RefSearchPage::new();
    let mut next = (INODE_REF_KEY, 0);
    let mut found = false;
    let mut previous_parent = ROOT_INODE;
    let mut path = Vec::new();
    while next.0 <= INODE_EXTREF_KEY {
        let (count, mut bytes) = page.read(fd, ino, next, INODE_EXTREF_KEY, u32::MAX)?;
        if count == 0 {
            break;
        }
        for _ in 0..count {
            let (key, mut item) = take_item(&mut bytes, ino, next, INODE_EXTREF_KEY)?;
            while !item.is_empty() {
                let (parent, name) = take_reference(key, &mut item)?;
                if parent != previous_parent {
                    directory_path(&mut directory_page, fd, parent, max_path_bytes, &mut path)?;
                    previous_parent = parent;
                }
                let parent_len = path.len();
                if name.len() + usize::from(parent_len != 0)
                    > max_path_bytes.saturating_sub(parent_len)
                {
                    return Err(path_too_long().into());
                }
                if parent_len != 0 {
                    path.push(b'/');
                }
                path.extend_from_slice(name);
                visit(&path)?;
                path.truncate(parent_len);
                found = true;
            }
            next = match key.1.checked_add(1) {
                Some(offset) => (key.0, offset),
                None => (key.0 + 1, 0),
            };
        }
    }
    if !found {
        return Err(
            BtrfsError::new("Btrfs returned no references for an overflowing inode").into(),
        );
    }
    Ok(())
}
