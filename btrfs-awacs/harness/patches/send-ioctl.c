// SPDX-License-Identifier: GPL-2.0
#define _GNU_SOURCE
/*
 * Minimal BTRFS_IOC_SEND driver for testing experimental send flags before
 * btrfs-progs grows command-line options for them.
 *
 * Usage: send-ioctl MODE SNAPSHOT PARENT_ROOT_ID OUTPUT
 */

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/btrfs.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef BTRFS_SEND_FLAG_NO_CLONE
#define BTRFS_SEND_FLAG_NO_CLONE 0x20
#endif
#ifndef BTRFS_SEND_FLAG_DISCARD_COMMANDS
#define BTRFS_SEND_FLAG_DISCARD_COMMANDS 0x40
#endif
#ifndef BTRFS_SEND_FLAG_CHANGED_OBJECTS
#define BTRFS_SEND_FLAG_CHANGED_OBJECTS 0x100
#endif

static void usage(const char *program)
{
	fprintf(stderr,
		"usage: %s {profile-default|profile-default-v2|no-clone|"
		"discard-commands|changed-objects} "
		"SNAPSHOT PARENT_ROOT_ID OUTPUT\n",
		program);
}

static int copy_stream_read_write(int input_fd, int output_fd)
{
	char buffer[64 * 1024];

	for (;;) {
		ssize_t bytes_read;
		ssize_t offset;

		bytes_read = read(input_fd, buffer, sizeof(buffer));
		if (bytes_read < 0) {
			if (errno == EINTR)
				continue;
			fprintf(stderr, "read send stream: %s\n", strerror(errno));
			return -1;
		}
		if (!bytes_read)
			return 0;

		offset = 0;
		while (offset < bytes_read) {
			ssize_t bytes_written;

			bytes_written = write(output_fd, buffer + offset,
					      bytes_read - offset);
			if (bytes_written < 0) {
				if (errno == EINTR)
					continue;
				fprintf(stderr, "write send stream: %s\n",
					strerror(errno));
				return -1;
			}
			if (!bytes_written) {
				fprintf(stderr, "short write of send stream\n");
				return -1;
			}
			offset += bytes_written;
		}
	}
}

static int copy_stream(int input_fd, int output_fd)
{
	bool first = true;

	for (;;) {
		ssize_t bytes;

		bytes = splice(input_fd, NULL, output_fd, NULL, 1024 * 1024,
			       SPLICE_F_MOVE);
		if (bytes < 0) {
			if (errno == EINTR)
				continue;
			if (first && errno == EINVAL)
				return copy_stream_read_write(input_fd, output_fd);
			fprintf(stderr, "splice send stream: %s\n",
				strerror(errno));
			return -1;
		}
		if (!bytes)
			return 0;
		first = false;
	}
}

int main(int argc, char **argv)
{
	struct btrfs_ioctl_send_args args = { 0 };
	char *end;
	uintmax_t parent_root;
	int pipe_fds[2];
	int output_fd;
	int snapshot_fd;
	int saved_errno;
	int child_status;
	pid_t child;
	int ret;

	if (argc != 5) {
		usage(argv[0]);
		return EXIT_FAILURE;
	}

	if (!strcmp(argv[1], "profile-default")) {
		args.flags = BTRFS_SEND_FLAG_NO_FILE_DATA;
	} else if (!strcmp(argv[1], "profile-default-v2")) {
		args.flags = BTRFS_SEND_FLAG_NO_FILE_DATA |
			     BTRFS_SEND_FLAG_VERSION;
		args.version = 2;
	} else if (!strcmp(argv[1], "no-clone")) {
		args.flags = BTRFS_SEND_FLAG_NO_FILE_DATA |
			     BTRFS_SEND_FLAG_NO_CLONE;
	} else if (!strcmp(argv[1], "discard-commands")) {
		args.flags = BTRFS_SEND_FLAG_NO_FILE_DATA |
			     BTRFS_SEND_FLAG_DISCARD_COMMANDS;
	} else if (!strcmp(argv[1], "changed-objects")) {
		args.flags = BTRFS_SEND_FLAG_NO_FILE_DATA |
			     BTRFS_SEND_FLAG_CHANGED_OBJECTS;
	} else {
		fprintf(stderr, "unknown send mode: %s\n", argv[1]);
		usage(argv[0]);
		return EXIT_FAILURE;
	}

	errno = 0;
	parent_root = strtoumax(argv[3], &end, 0);
	if (errno || end == argv[3] || *end != '\0' || !parent_root ||
	    parent_root > UINT64_MAX) {
		fprintf(stderr, "invalid parent root ID: %s\n", argv[3]);
		return EXIT_FAILURE;
	}

	snapshot_fd = open(argv[2], O_RDONLY | O_DIRECTORY);
	if (snapshot_fd < 0) {
		fprintf(stderr, "open %s: %s\n", argv[2], strerror(errno));
		return EXIT_FAILURE;
	}

	output_fd = open(argv[4], O_WRONLY | O_CREAT | O_TRUNC, 0600);
	if (output_fd < 0) {
		fprintf(stderr, "open %s: %s\n", argv[4], strerror(errno));
		close(snapshot_fd);
		return EXIT_FAILURE;
	}

	if (pipe2(pipe_fds, O_CLOEXEC) < 0) {
		fprintf(stderr, "pipe: %s\n", strerror(errno));
		close(output_fd);
		close(snapshot_fd);
		return EXIT_FAILURE;
	}

	child = fork();
	if (child < 0) {
		fprintf(stderr, "fork: %s\n", strerror(errno));
		close(pipe_fds[0]);
		close(pipe_fds[1]);
		close(output_fd);
		close(snapshot_fd);
		return EXIT_FAILURE;
	}
	if (!child) {
		close(pipe_fds[1]);
		close(snapshot_fd);
		ret = copy_stream(pipe_fds[0], output_fd);
		close(pipe_fds[0]);
		close(output_fd);
		_exit(ret ? EXIT_FAILURE : EXIT_SUCCESS);
	}

	close(pipe_fds[0]);
	close(output_fd);

	args.send_fd = pipe_fds[1];
	args.parent_root = parent_root;

	ret = ioctl(snapshot_fd, BTRFS_IOC_SEND, &args);
	saved_errno = errno;
	close(pipe_fds[1]);
	if (ret < 0)
		fprintf(stderr, "BTRFS_IOC_SEND: %s\n", strerror(saved_errno));

	if (waitpid(child, &child_status, 0) < 0) {
		fprintf(stderr, "waitpid: %s\n", strerror(errno));
		ret = -1;
	} else if (!WIFEXITED(child_status) ||
		   WEXITSTATUS(child_status) != EXIT_SUCCESS) {
		fprintf(stderr, "send stream copier failed\n");
		ret = -1;
	}

	close(snapshot_fd);
	return ret < 0 ? EXIT_FAILURE : EXIT_SUCCESS;
}
