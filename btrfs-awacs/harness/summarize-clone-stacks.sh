#!/usr/bin/env bash
set -euo pipefail

if (( $# != 2 )); then
	echo "usage: $0 LABEL PERF_DATA" >&2
	exit 2
fi

label=$1
perf_data=$2

if [[ ! -r "$perf_data" ]]; then
	echo "error: cannot read perf data: $perf_data" >&2
	exit 1
fi

printf 'variant\ttotal\tsend_utimes\tbtrfs_lookup_xattr\ttree_move_down\treplace_node_with_clone\tget_first_ref\tdir_item_lookup\tinode_info\tother\n'

perf script -i "$perf_data" |
	awk -v label="$label" '
	function flush_sample()
	{
		if (clone) {
			total++
			classified = 0
			if (send_utimes) {
				utimes++
				classified = 1
			}
			if (lookup_xattr) {
				xattr++
				classified = 1
			}
			if (tree_move_down) {
				tree++
				classified = 1
			}
			if (replace_node) {
				replace++
				classified = 1
			}
			if (first_ref) {
				first++
				classified = 1
			}
			if (dir_item) {
				dir++
				classified = 1
			}
			if (inode_info) {
				inode++
				classified = 1
			}
			if (!classified)
				other++
		}

		clone = 0
		send_utimes = 0
		lookup_xattr = 0
		tree_move_down = 0
		replace_node = 0
		first_ref = 0
		dir_item = 0
		inode_info = 0
	}

	/^$/ {
		flush_sample()
		next
	}

	/btrfs_clone_extent_buffer/	{ clone = 1 }
	/send_utimes/			{ send_utimes = 1 }
	/btrfs_lookup_xattr/		{ lookup_xattr = 1 }
	/tree_move_down/		{ tree_move_down = 1 }
	/replace_node_with_clone/	{ replace_node = 1 }
	/get_first_ref/			{ first_ref = 1 }
	/lookup_dir_item_(key|inode)/	{ dir_item = 1 }
	/(get|read)_inode_info/		{ inode_info = 1 }

	END {
		flush_sample()
		printf "%s\t%d\t%d\t%d\t%d\t%d\t%d\t%d\t%d\t%d\n",
		       label, total, utimes, xattr, tree, replace, first, dir,
		       inode, other
	}
	'
