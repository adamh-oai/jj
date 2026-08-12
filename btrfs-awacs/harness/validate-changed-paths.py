#!/usr/bin/env python3
"""Validate a compact changed-path manifest against a v1 no-data send stream."""

import argparse
import struct
from pathlib import Path


STREAM_MAGIC = b"btrfs-stream\0"
MANIFEST_MAGIC = b"btrfs-paths\0"

CMD_RENAME = 9
ATTR_PATH = 15
ATTR_PATH_TO = 16
PATH_COMMANDS = {
    3,  # MKFILE
    5,  # MKNOD
    8,  # SYMLINK
    10,  # LINK
    11,  # UNLINK
    13,  # SET_XATTR
    14,  # REMOVE_XATTR
    15,  # WRITE
    16,  # CLONE
    17,  # TRUNCATE
    18,  # CHMOD
    19,  # CHOWN
    20,  # UTIMES
    22,  # UPDATE_EXTENT
}


def selected_path(cmd: int, attr: int) -> bool:
    if cmd == CMD_RENAME:
        return attr == ATTR_PATH_TO
    return cmd in PATH_COMMANDS and attr == ATTR_PATH


def parse_send_stream(path: Path) -> tuple[dict[bytes, int], int]:
    data = path.read_bytes()
    header_len = len(STREAM_MAGIC) + 4
    if data[: len(STREAM_MAGIC)] != STREAM_MAGIC:
        raise ValueError("invalid Btrfs stream magic")
    (version,) = struct.unpack_from("<I", data, len(STREAM_MAGIC))
    if version != 1:
        raise ValueError(f"expected stream version 1, got {version}")

    paths: dict[bytes, int] = {}
    selected_tlvs = 0
    offset = header_len
    while offset < len(data):
        if offset + 10 > len(data):
            raise ValueError("truncated command header")
        payload_len, cmd, _crc = struct.unpack_from("<IHI", data, offset)
        offset += 10
        command_end = offset + payload_len
        if command_end > len(data):
            raise ValueError("truncated command payload")

        while offset < command_end:
            if offset + 4 > command_end:
                raise ValueError("truncated TLV header")
            attr, attr_len = struct.unpack_from("<HH", data, offset)
            offset += 4
            attr_end = offset + attr_len
            if attr_end > command_end:
                raise ValueError("truncated TLV payload")
            if selected_path(cmd, attr):
                value = data[offset:attr_end]
                paths[value] = paths.get(value, 0) | (1 << cmd)
                selected_tlvs += 1
            offset = attr_end

    if offset != len(data):
        raise ValueError("trailing partial command")
    return paths, selected_tlvs


def parse_manifest(path: Path) -> dict[bytes, int]:
    data = path.read_bytes()
    if len(data) < 24:
        raise ValueError("truncated manifest header")
    magic, version, count = struct.unpack_from("<12sIQ", data)
    if magic != MANIFEST_MAGIC:
        raise ValueError("invalid manifest magic")
    if version != 1:
        raise ValueError(f"expected manifest version 1, got {version}")

    paths: dict[bytes, int] = {}
    offset = 24
    for _ in range(count):
        if offset + 12 > len(data):
            raise ValueError("truncated manifest record")
        commands, path_len = struct.unpack_from("<QI", data, offset)
        offset += 12
        path_end = offset + path_len
        if path_end > len(data):
            raise ValueError("truncated manifest path")
        value = data[offset:path_end]
        if value in paths:
            raise ValueError(f"duplicate manifest path: {value!r}")
        paths[value] = commands
        offset = path_end

    if offset != len(data):
        raise ValueError(f"{len(data) - offset} trailing manifest bytes")
    return paths


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("send_stream", type=Path)
    parser.add_argument("manifest", type=Path)
    args = parser.parse_args()

    expected, selected_tlvs = parse_send_stream(args.send_stream)
    actual = parse_manifest(args.manifest)
    if actual != expected:
        missing = expected.keys() - actual.keys()
        extra = actual.keys() - expected.keys()
        wrong_mask = {
            path
            for path in expected.keys() & actual.keys()
            if expected[path] != actual[path]
        }
        raise SystemExit(
            "manifest mismatch: "
            f"missing={len(missing)} extra={len(extra)} "
            f"wrong_command_mask={len(wrong_mask)}"
        )

    print(
        f"validated records={len(actual)} selected_path_tlvs={selected_tlvs} "
        f"manifest_bytes={args.manifest.stat().st_size}"
    )


if __name__ == "__main__":
    main()
