// Included in manager::tests so the benchmark uses the same isolated catalog
// fixtures as the publication tests, without requiring Btrfs or a broker.
//
// AWACS_PUBLICATION_BENCHMARK_SPOOL=/path/to/retained/replay.spool \
//   cargo test -p btrfs-awacs --lib --profile dev-opt \
//   benchmark_spooled_delta_publication -- --ignored --nocapture
#[test]
#[ignore = "requires a retained v2 replay spool supplied through the environment"]
fn benchmark_spooled_delta_publication() {
    let source_path = std::env::var_os("AWACS_PUBLICATION_BENCHMARK_SPOOL")
        .expect("set AWACS_PUBLICATION_BENCHMARK_SPOOL to a retained v2 replay spool");
    let repeats = std::env::var("AWACS_PUBLICATION_BENCHMARK_REPEATS")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(3);
    assert!((1..=20).contains(&repeats));
    let bytes = fs::read(source_path).unwrap();
    assert_eq!(&bytes[..16], crate::manifest::CHANGED_OBJECTS_V2_MAGIC);
    let mut manifest_len = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
    loop {
        let kind = u16::from_le_bytes(bytes[manifest_len..manifest_len + 2].try_into().unwrap());
        let length = u32::from_le_bytes(
            bytes[manifest_len + 4..manifest_len + 8]
                .try_into()
                .unwrap(),
        ) as usize;
        assert!(length >= 8);
        manifest_len = manifest_len.checked_add(length).unwrap();
        assert!(manifest_len <= bytes.len());
        if kind == 0xffff {
            break;
        }
    }
    let manifest = crate::manifest::parse_changed_objects_v2(&bytes[..manifest_len])
        .unwrap()
        .manifest;
    let trailer = &bytes[bytes.len() - REPLAY_SPOOL_TRAILER_LEN..];
    assert_eq!(&trailer[..16], REPLAY_SPOOL_TRAILER_MAGIC);
    let payload_len = u64::from_le_bytes(trailer[16..24].try_into().unwrap()) as usize;
    let payload_offset = bytes.len() - REPLAY_SPOOL_TRAILER_LEN - payload_len;
    let event_count = u64::from_le_bytes(
        bytes[payload_offset..payload_offset + 8]
            .try_into()
            .unwrap(),
    );
    let hash: [u8; 32] = trailer[24..56].try_into().unwrap();
    let canonical_hash = manifest.canonical_hash();
    for repeat in 0..repeats {
        let (temp, mut store, request) = setup();
        let (_, watch) = initialize_watch(&mut store, &request);
        let (cut_request, cut, recorded) =
            record_test_cut(&mut store, &watch, request.source_subvol_uuid, 1);
        let spool_path = temp.path().join("replay.spool");
        fs::write(&spool_path, &bytes).unwrap();
        let events = load_replay_spool_payload(
            spool_path.as_os_str().as_bytes(),
            hash,
            i64::try_from(event_count).unwrap(),
        )
        .unwrap();
        // Only publication is timed. Parsing, copying the immutable fixture,
        // initializing the catalog and recording the cut happen beforehand.
        let changes_before = store.connection().total_changes();
        let started = std::time::Instant::now();
        let published = store
            .publish_adjacent_broker_delta(
                &cut,
                cut_request.lease_owner,
                &recorded,
                &manifest,
                &events,
                Some((spool_path.as_os_str().as_bytes(), hash)),
                cut_request.now_ns + 4,
            )
            .unwrap();
        let elapsed = started.elapsed();
        let sqlite_changes = store.connection().total_changes() - changes_before;
        assert_eq!(published.events, events);
        let catalog_path = store.path().to_owned();
        drop(store);
        let mut reopened = Store::open(&catalog_path).unwrap();
        let stored_hash: Vec<u8> = reopened
            .connection()
            .query_row(
                "SELECT manifest_hash FROM comparisons WHERE id = ?1",
                [published.comparison_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_hash, canonical_hash);
        let replay = reopened
            .replay_historical_changes(
                watch.watch_id,
                watch.grant_id,
                1000,
                watch.snapshot_identity.subvol_uuid,
                recorded.identity.subvol_uuid,
            )
            .unwrap();
        assert!(!replay.fresh_instance);
        assert_eq!(replay.events, events);
        assert!(reopened.foreign_key_violations().unwrap().is_empty());
        eprintln!(
            "publication_benchmark repeat={repeat} objects={} refs={} events={} elapsed_us={} \
             sqlite_changes={sqlite_changes}",
            manifest.objects.len(),
            manifest.ref_adds.len() + manifest.ref_deletes.len(),
            events.len(),
            elapsed.as_micros(),
        );
    }
}
