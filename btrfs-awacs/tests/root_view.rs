use std::ffi::OsString;
use std::fs::File;
use std::fs::{self};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use btrfs_awacs::btrfs::OpenedSubvolume;
use btrfs_awacs::btrfs::{self};
use btrfs_awacs::facade::FacadeService;
use btrfs_awacs::manager::InitializedWatch;
use btrfs_awacs::manager::PERMISSION_CUT;
use btrfs_awacs::manager::PERMISSION_READ;
use btrfs_awacs::manager::Permissions;
use btrfs_awacs::manager::Principal;
use btrfs_awacs::namespace::RootView;
use btrfs_awacs::service::ChangesOptions;
use btrfs_awacs::service::InitializeOptions;
use btrfs_awacs::service::Service;
use btrfs_awacs::service::ServiceConfig;
use btrfs_awacs::store::ServiceMetadata;
use btrfs_awacs::store::Store;

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
            .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
        Self(tempfile::tempdir_in(root).unwrap())
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn source(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let source = self.path().join(relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let output = Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::write(source.join("contents"), contents).unwrap();
        source
    }

    fn snapshot(&self, view: &RootView, name: &str) -> PathBuf {
        let parent = File::open(self.path()).unwrap();
        btrfs::create_snapshot(view.source().as_fd(), parent.as_fd(), name.as_bytes(), true)
            .unwrap();
        let path = self.path().join(name);
        let snapshot = OpenedSubvolume::open(&path).unwrap();
        assert!(snapshot.subvolume.readonly());
        assert_eq!(
            snapshot.subvolume.parent_uuid,
            Some(view.source().subvolume.uuid)
        );
        path
    }

    fn service(&self, source: &Path) -> (Service, InitializedWatch) {
        let broker = std::env::var_os("BTRFS_AWACS_BROKER_DIR")
            .or_else(|| std::env::var_os("BTRFS_AWACS_BROKER_SOCKET"))
            .map(PathBuf::from)
            .expect("BTRFS_AWACS_BROKER_DIR must name a matching live AWACS broker");
        let managed = self.path().join("managed");
        let spool = self.path().join("spool");
        for path in [&managed, &spool] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let boot_id = [23; 16];
        let metadata = ServiceMetadata::generate(boot_id, now_ns()).unwrap();
        let store = Store::create(&self.path().join("state.sqlite3"), &metadata).unwrap();
        let config = ServiceConfig::new(managed, spool, boot_id).with_broker_directory(broker);
        let mut service = Service::new_external(store, config).unwrap();
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let initialized = service
            .initialize(
                source,
                &InitializeOptions {
                    principal: Principal::Uid(u64::from(uid)),
                    permissions: Permissions::new(PERMISSION_READ | PERMISSION_CUT).unwrap(),
                    requester_uid: uid,
                    requester_gid: gid,
                    now_ns: now_ns(),
                },
            )
            .unwrap();
        (service, initialized)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The broker owns this empty directory. Removing it through its
        // writable parent does not require traversing the directory.
        let _ = fs::remove_dir(self.path().join("managed/.broker-trash-default"));
        if let Err(error) = btrfs::remove_tree(self.path()) {
            if std::thread::panicking() {
                eprintln!("failed to clean Btrfs root-view fixture: {error}");
            } else {
                panic!("failed to clean Btrfs root-view fixture: {error}");
            }
        }
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .try_into()
        .unwrap()
}

fn changes_options(watch: &InitializedWatch) -> ChangesOptions {
    ChangesOptions {
        watch_id: watch.watch_id,
        authorization_id: watch.grant_id,
        requester_uid: unsafe { libc::geteuid() },
        requester_gid: unsafe { libc::getegid() },
        now_ns: now_ns(),
    }
}

fn snapshot_path(service: &Service, snapshot_id: i64) -> PathBuf {
    let path: Vec<u8> = service
        .store()
        .connection()
        .query_row(
            "SELECT path FROM snapshots WHERE id = ?1 AND physical_state = 'present'",
            [snapshot_id],
            |row| row.get(0),
        )
        .unwrap();
    PathBuf::from(OsString::from_vec(path))
}

#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT and Btrfs snapshot permissions"]
fn retained_source_survives_ancestor_rename() {
    let fixture = Fixture::new();
    let source = fixture.source("parent/source", b"original");
    let view = RootView::open(&source).unwrap();

    fs::rename(fixture.path().join("parent"), fixture.path().join("moved")).unwrap();
    assert!(!source.exists());

    let snapshot = fixture.snapshot(&view, "snapshot");
    assert_eq!(fs::read(snapshot.join("contents")).unwrap(), b"original");
}

#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT and Btrfs snapshot permissions"]
fn retained_source_ignores_replacement_at_original_path() {
    let fixture = Fixture::new();
    let source = fixture.source("source", b"original");
    let view = RootView::open(&source).unwrap();

    fs::rename(&source, fixture.path().join("moved")).unwrap();
    fixture.source("source", b"replacement");
    let replacement = RootView::open(&source).unwrap();
    assert!(!view.binding().same_root(replacement.binding()));

    let snapshot = fixture.snapshot(&view, "snapshot");
    assert_eq!(fs::read(snapshot.join("contents")).unwrap(), b"original");
    assert_eq!(fs::read(source.join("contents")).unwrap(), b"replacement");
}

#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT and Btrfs snapshot permissions"]
fn root_identity_accepts_rename_away_and_back() {
    let fixture = Fixture::new();
    let source = fixture.source("source", b"original");
    let view = RootView::open(&source).unwrap();

    let moved = fixture.path().join("moved");
    fs::rename(&source, &moved).unwrap();
    fs::rename(&moved, &source).unwrap();
    let reopened = RootView::open(&source).unwrap();
    assert!(view.binding().same_root(reopened.binding()));

    let snapshot = fixture.snapshot(&view, "snapshot");
    assert_eq!(fs::read(snapshot.join("contents")).unwrap(), b"original");
}

#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT, changed-objects v2 kernel support, and matching broker"]
fn service_cut_uses_retained_source_after_path_replacement() {
    let fixture = Fixture::new();
    let source = fixture.source("source", b"baseline");
    let (mut service, watch) = fixture.service(&source);
    let view = RootView::open(&source).unwrap();

    let moved = fixture.path().join("moved");
    fs::rename(&source, &moved).unwrap();
    fixture.source("source", b"replacement");
    fs::write(moved.join("contents"), b"updated original").unwrap();

    let cut = service
        .changes(view.source(), &changes_options(&watch))
        .unwrap();
    assert_eq!(cut.sequence, watch.sequence + 1);
    let path = snapshot_path(&service, cut.snapshot_id);
    assert_eq!(
        fs::read(path.join("contents")).unwrap(),
        b"updated original"
    );
    let snapshot = OpenedSubvolume::open(&path).unwrap();
    assert_eq!(
        snapshot.subvolume.parent_uuid,
        Some(view.source().subvolume.uuid)
    );
    assert_eq!(fs::read(source.join("contents")).unwrap(), b"replacement");
}

#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT, changed-objects v2 kernel support, and matching broker"]
fn service_rejects_wrong_source_without_starting_a_cut() {
    let fixture = Fixture::new();
    let source = fixture.source("source", b"baseline");
    let other = fixture.source("other", b"unrelated");
    let (mut service, watch) = fixture.service(&source);
    let wrong = RootView::open(&other).unwrap();
    let counts = |service: &Service| -> (i64, i64) {
        service
            .store()
            .connection()
            .query_row(
                "SELECT (SELECT count(*) FROM operations), (SELECT count(*) FROM snapshots)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    let before = counts(&service);

    assert!(
        service
            .changes(wrong.source(), &changes_options(&watch))
            .is_err()
    );
    assert_eq!(counts(&service), before);

    fs::write(source.join("contents"), b"correct update").unwrap();
    let correct = RootView::open(&source).unwrap();
    let cut = service
        .changes(correct.source(), &changes_options(&watch))
        .unwrap();
    assert_eq!(cut.sequence, watch.sequence + 1);
    assert_eq!(
        fs::read(snapshot_path(&service, cut.snapshot_id).join("contents")).unwrap(),
        b"correct update"
    );
}

#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT, changed-objects v2 kernel support, and matching broker"]
fn active_facade_keeps_retained_source_after_ancestor_and_path_replacement() {
    let fixture = Fixture::new();
    let source = fixture.source("parent/source", b"baseline");
    let (service, watch) = fixture.service(&source);
    let view = RootView::open(&source).unwrap();
    let mut facade = FacadeService::new(service);
    facade
        .activate(watch.watch_id, watch.grant_id, &view)
        .unwrap();

    fs::rename(fixture.path().join("parent"), fixture.path().join("moved")).unwrap();
    fixture.source("parent/source", b"replacement");
    fs::write(
        fixture.path().join("moved/source/contents"),
        b"updated original",
    )
    .unwrap();

    let options = changes_options(&watch);
    let prepared = facade
        .prepare_scan_query(
            watch.watch_id,
            &view,
            None,
            options.requester_uid,
            options.requester_gid,
            options.now_ns,
            options.now_ns + 300_000_000_000,
            false,
        )
        .unwrap();
    let path = facade.prepared_snapshot_path(&prepared).unwrap();
    assert_eq!(
        fs::read(path.join("contents")).unwrap(),
        b"updated original"
    );
    let snapshot = OpenedSubvolume::open(&path).unwrap();
    assert_eq!(
        snapshot.subvolume.parent_uuid,
        Some(view.source().subvolume.uuid)
    );

    let replacement = RootView::open(&source).unwrap();
    let options = changes_options(&watch);
    let error = facade
        .prepare_scan_query(
            watch.watch_id,
            &replacement,
            None,
            options.requester_uid,
            options.requester_gid,
            options.now_ns,
            options.now_ns + 300_000_000_000,
            false,
        )
        .err()
        .expect("the replacement must not reuse the original root's active view");
    assert!(error.to_string().contains("does not match its active view"));
    assert_eq!(facade.view_binding(watch.watch_id), Some(view.binding()));

    // Rejecting another root must not invalidate the original response or
    // its lease. Completing it still checks the original active view.
    let result = facade.finish_query_response(prepared).unwrap();
    assert_eq!(result.sequence, watch.sequence + 1);
    assert_eq!(fs::read(source.join("contents")).unwrap(), b"replacement");
}
