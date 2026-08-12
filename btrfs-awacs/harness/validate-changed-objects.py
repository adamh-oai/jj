#!/usr/bin/env python3
"""Validate and summarize an experimental Btrfs changed-object stream."""

import argparse
import struct
from collections import Counter
from pathlib import Path


MAGIC = b"btrfs-changes\0\0\0"
V2_MAGIC = b"btrfs-objects-v2"
HEADER_SIZE = 24
V2_HEADER_SIZE = 112
OBJECT_RECORD_SIZE = 40
V2_OBJECT_RECORD_SIZE = 96
REF_RECORD_SIZE = 24

RECORD_OBJECT = 1
RECORD_REF_ADD = 2
RECORD_REF_DELETE = 3
RECORD_XATTR_RESET = 4
RECORD_XATTR = 5
RECORD_BOUNDARY_ADD = 6
RECORD_BOUNDARY_DELETE = 7
RECORD_COMPLETION = 0xFFFF
RECORD_TARGET_VALID = 1
RECORD_OPTIONAL = 1 << 15

OBJECT_MASKS = {
    1 << 0: "inode",
    1 << 1: "ref",
    1 << 2: "xattr",
    1 << 3: "data",
    1 << 4: "verity",
    1 << 5: "created",
    1 << 6: "deleted",
}
VALID_OBJECT_MASK = sum(OBJECT_MASKS)


def crc32c(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return (~crc) & 0xFFFFFFFF


def parse_v1(data: bytes):
    if len(data) < HEADER_SIZE:
        raise ValueError("truncated header")
    if data[:16] != MAGIC:
        raise ValueError("invalid magic")
    version, header_len = struct.unpack_from("<II", data, 16)
    if version != 1 or header_len != HEADER_SIZE:
        raise ValueError(f"unsupported header version={version} len={header_len}")

    objects: dict[int, tuple[int, int, int]] = {}
    ref_adds: set[tuple[int, int, bytes]] = set()
    ref_deletes: set[tuple[int, int, bytes]] = set()
    offset = header_len
    while offset < len(data):
        if offset + 8 > len(data):
            raise ValueError("truncated record header")
        record_type, flags, record_len = struct.unpack_from("<HHI", data, offset)
        if flags:
            raise ValueError(f"unsupported record flags {flags:#x}")
        if record_len < 8 or offset + record_len > len(data):
            raise ValueError(f"invalid record length {record_len}")
        record_end = offset + record_len

        if record_type == RECORD_OBJECT:
            if record_len != OBJECT_RECORD_SIZE:
                raise ValueError(f"invalid object length {record_len}")
            ino, old_gen, new_gen, mask = struct.unpack_from("<QQQQ", data, offset + 8)
            if mask & ((1 << 5) | (1 << 6)) and not mask & (1 << 0):
                raise ValueError("created or deleted object has no inode change")
            if not ino or not mask or mask & ~VALID_OBJECT_MASK:
                raise ValueError("invalid object fields")
            if mask & (1 << 5) and not new_gen:
                raise ValueError("created object has no new generation")
            if mask & (1 << 6) and not old_gen:
                raise ValueError("deleted object has no old generation")
            if ino in objects:
                raise ValueError(f"duplicate object {ino}")
            objects[ino] = (old_gen, new_gen, mask)
        elif record_type in (RECORD_REF_ADD, RECORD_REF_DELETE):
            if not REF_RECORD_SIZE < record_len <= REF_RECORD_SIZE + 255:
                raise ValueError(f"invalid ref length {record_len}")
            ino, parent_ino = struct.unpack_from("<QQ", data, offset + 8)
            if not ino or not parent_ino:
                raise ValueError("invalid ref inode")
            name = data[offset + REF_RECORD_SIZE : record_end]
            if b"/" in name or b"\0" in name:
                raise ValueError("invalid ref name")
            record = (ino, parent_ino, name)
            records = ref_adds if record_type == RECORD_REF_ADD else ref_deletes
            if record in records:
                raise ValueError("duplicate reference")
            records.add(record)
        else:
            raise ValueError(f"unknown record type {record_type}")
        offset = record_end

    refs = ref_adds | ref_deletes
    for ino, _parent, _name in refs:
        if ino not in objects:
            raise ValueError("reference has no corresponding object")
        if not objects[ino][2] & (1 << 1):
            raise ValueError("reference object has no ref change")
    ref_inos = {ino for ino, _parent, _name in refs}
    if any(
        mask & (1 << 1) and ino not in ref_inos
        for ino, (_old, _new, mask) in objects.items()
    ):
        raise ValueError("ref object has no reference records")

    return objects, ref_adds, ref_deletes


def parse_v2(data: bytes):
    if len(data) < V2_HEADER_SIZE + 32 or data[:16] != V2_MAGIC:
        raise ValueError("truncated v2 header")
    version, header_len = struct.unpack_from("<II", data, 16)
    flags = struct.unpack_from("<Q", data, 24)[0]
    if (
        version != 2
        or header_len != V2_HEADER_SIZE
        or flags & ~7
        or not flags & (1 << 1)
        or not flags & (1 << 2)
    ):
        raise ValueError("unsupported v2 header")
    if not any(data[32:48]) or not any(data[64:80]):
        raise ValueError("v2 stream has empty endpoint identity")

    objects: dict[int, tuple[int, int, int]] = {}
    ref_adds: set[tuple[int, int, bytes]] = set()
    ref_deletes: set[tuple[int, int, bytes]] = set()
    xattr_resets: set[int] = set()
    boundary_adds: set[tuple[int, int, bytes]] = set()
    boundary_deletes: set[tuple[int, int, bytes]] = set()
    offset = V2_HEADER_SIZE
    records = 0
    while True:
        if offset + 8 > len(data):
            raise ValueError("v2 stream has no completion record")
        record_type, record_flags, record_len = struct.unpack_from("<HHI", data, offset)
        record_end = offset + record_len
        if record_len < 8 or record_end > len(data):
            raise ValueError(f"invalid v2 record length {record_len}")
        if record_type == RECORD_COMPLETION:
            if record_flags or record_len != 32 or record_end != len(data):
                raise ValueError("invalid v2 completion record")
            declared_records, declared_bytes, checksum, reserved = struct.unpack_from(
                "<QQII", data, offset + 8
            )
            if (
                declared_records != records
                or declared_bytes != offset
                or reserved
                or checksum != crc32c(data[:offset])
            ):
                raise ValueError("v2 completion mismatch")
            break
        if record_type == RECORD_OBJECT:
            if record_len != V2_OBJECT_RECORD_SIZE or record_flags & ~RECORD_TARGET_VALID:
                raise ValueError("invalid v2 object record")
            ino, old_gen, new_gen, mask = struct.unpack_from("<QQQQ", data, offset + 8)
            if not ino or not mask or mask & ~VALID_OBJECT_MASK or ino in objects:
                raise ValueError("invalid or duplicate v2 object")
            objects[ino] = (old_gen, new_gen, mask)
        elif record_type in (RECORD_REF_ADD, RECORD_REF_DELETE):
            if record_flags or not REF_RECORD_SIZE < record_len <= REF_RECORD_SIZE + 255:
                raise ValueError("invalid v2 reference")
            ino, parent_ino = struct.unpack_from("<QQ", data, offset + 8)
            name = data[offset + REF_RECORD_SIZE : record_end]
            if not ino or not parent_ino or not name or b"/" in name or b"\0" in name:
                raise ValueError("invalid v2 reference fields")
            record = (ino, parent_ino, name)
            destination = ref_adds if record_type == RECORD_REF_ADD else ref_deletes
            if record in destination:
                raise ValueError("duplicate v2 reference")
            destination.add(record)
        elif record_type == RECORD_XATTR_RESET:
            if record_flags or record_len != 16:
                raise ValueError("invalid v2 xattr reset")
            ino = struct.unpack_from("<Q", data, offset + 8)[0]
            if not ino or ino in xattr_resets:
                raise ValueError("duplicate v2 xattr reset")
            xattr_resets.add(ino)
        elif record_type == RECORD_XATTR:
            if record_flags or record_len <= 24:
                raise ValueError("invalid v2 xattr")
            ino, name_len, value_len = struct.unpack_from("<QII", data, offset + 8)
            if ino not in xattr_resets or not name_len or 24 + name_len + value_len != record_len:
                raise ValueError("invalid v2 xattr fields")
        elif record_type in (RECORD_BOUNDARY_ADD, RECORD_BOUNDARY_DELETE):
            if record_flags or not REF_RECORD_SIZE < record_len <= REF_RECORD_SIZE + 255:
                raise ValueError("invalid v2 boundary")
            parent_ino, child_root_id = struct.unpack_from("<QQ", data, offset + 8)
            name = data[offset + REF_RECORD_SIZE : record_end]
            if (
                not parent_ino
                or not child_root_id
                or not name
                or b"/" in name
                or b"\0" in name
            ):
                raise ValueError("invalid v2 boundary fields")
            record = (parent_ino, child_root_id, name)
            destination = (
                boundary_adds
                if record_type == RECORD_BOUNDARY_ADD
                else boundary_deletes
            )
            if record in destination:
                raise ValueError("duplicate v2 boundary")
            destination.add(record)
        elif record_flags != RECORD_OPTIONAL:
            raise ValueError(f"unknown mandatory v2 record type {record_type}")
        records += 1
        offset = record_end

    for ino, _parent, _name in ref_adds | ref_deletes:
        if ino not in objects or not objects[ino][2] & (1 << 1):
            raise ValueError("v2 reference lacks a ref object")
    for ino in xattr_resets:
        if ino not in objects or objects[ino][2] & (1 << 6):
            raise ValueError("v2 xattrs lack a surviving object")
    return objects, ref_adds, ref_deletes, boundary_adds, boundary_deletes


def parse(path: Path):
    data = path.read_bytes()
    if data[:16] == V2_MAGIC:
        objects, ref_adds, ref_deletes, boundary_adds, boundary_deletes = parse_v2(data)
    else:
        objects, ref_adds, ref_deletes = parse_v1(data)
        boundary_adds, boundary_deletes = set(), set()
    return data, objects, ref_adds, ref_deletes, boundary_adds, boundary_deletes


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("manifest", type=Path)
    args = parser.parse_args()

    data, objects, ref_adds, ref_deletes, boundary_adds, boundary_deletes = parse(
        args.manifest
    )
    masks = Counter()
    for _old_gen, _new_gen, mask in objects.values():
        for bit, name in OBJECT_MASKS.items():
            if mask & bit:
                masks[name] += 1

    net_adds = ref_adds - ref_deletes
    net_deletes = ref_deletes - ref_adds
    net_boundary_adds = boundary_adds - boundary_deletes
    net_boundary_deletes = boundary_deletes - boundary_adds
    mask_text = " ".join(f"{name}={masks[name]}" for name in OBJECT_MASKS.values())
    print(
        f"validated bytes={len(data)} objects={len(objects)} "
        f"raw_refs=+{len(ref_adds)}/-{len(ref_deletes)} "
        f"net_refs=+{len(net_adds)}/-{len(net_deletes)} "
        f"boundaries=+{len(net_boundary_adds)}/-{len(net_boundary_deletes)} "
        f"{mask_text}"
    )


if __name__ == "__main__":
    main()
