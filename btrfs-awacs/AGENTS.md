# Local workflow

This directory is the AWACS-specific implementation and documentation surface.
Prefer the narrowest validation that covers the edited code.

- Run `just test` for ordinary changes limited to `btrfs-awacs/`. It sets
  the expected terminal environment and runs only `cargo test -p
  btrfs-awacs`.
- Run `just test-workspace` only when the change also touches JJ code outside
  this directory, such as `cli/`, `lib/`, or shared workspace behavior.
- Use `just install` for most live testing. It delegates to
  `./install.sh dev-opt`, which builds JJ and AWACS, installs both binaries,
  installs the Git fsmonitor entry point, and restarts the broker.
- Use `just install-release` only for final release/profile-specific
  validation. Do not introduce a separate `CARGO_TARGET_DIR` for routine
  work; reuse the workspace's normal `target/` cache. An explicitly supplied
  `CARGO_TARGET_DIR` is still respected when isolation is intentional.
- Run `just test-e2e` for the real Btrfs Git worktree/JJ adoption smoke test.
  It defaults to the installed dev-opt binaries and broker, while honoring
  `JJ_TEST_BTRFS_ROOT`, `BTRFS_AWACS_COMMAND`, and
  `BTRFS_AWACS_BROKER_SOCKET` overrides. Real Btrfs smoke tests are in addition
  to `just test`, not a replacement for it.
