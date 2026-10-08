#![cfg(target_os = "macos")]
use bree_cli::collect::Collector;
use bree_cli::model::{SCHEMA_VERSION, Validity};
use std::collections::HashSet;

#[test]
fn live_snapshot_has_real_core_metrics_and_full_membership() {
    let mut collector = Collector::new().unwrap();
    let snapshot = collector.snapshot().unwrap();
    assert_eq!(snapshot.schema_version, SCHEMA_VERSION);
    assert!(
        snapshot
            .system
            .total_bytes
            .value
            .is_some_and(|value| value > 0)
    );
    assert!(
        snapshot
            .system
            .used_bytes
            .value
            .is_some_and(|value| value > 0)
    );
    let own = snapshot
        .processes
        .iter()
        .find(|process| process.identity.pid == std::process::id())
        .unwrap();
    assert_eq!(own.identity.status, Validity::Ok);
    assert!(own.memory_bytes.value.is_some_and(|value| value > 0));
    assert_eq!(own.metric_kind, "rss");
    assert!(own.cpu_one_core_percent.value.is_none());
    let mut grouped = HashSet::new();
    for group in &snapshot.groups {
        for id in &group.process_ids {
            assert!(
                grouped.insert(id.clone()),
                "a process belongs to only one group"
            );
        }
    }
    assert_eq!(grouped.len(), snapshot.processes.len());
    for process in &snapshot.processes {
        if process.memory_bytes.status != Validity::Ok {
            assert_eq!(process.memory_bytes.value, None);
        }
        if process.cpu_one_core_percent.status != Validity::Ok {
            assert_eq!(process.cpu_one_core_percent.value, None);
        }
    }
}

#[test]
fn collector_can_move_to_a_joined_worker_without_appkit_objects() {
    fn assert_send<T: Send>() {}
    assert_send::<Collector>();
    let mut collector = Collector::new().unwrap();
    let worker = std::thread::spawn(move || collector.snapshot());
    let snapshot = worker
        .join()
        .expect("sampling worker must not panic")
        .unwrap();
    assert!(!snapshot.processes.is_empty());
}

#[test]
fn committed_memory_load_is_observed_in_the_same_instance() {
    let mut collector = Collector::new().unwrap();
    let before = collector.snapshot().unwrap();
    let own_before = before
        .processes
        .iter()
        .find(|p| p.identity.pid == std::process::id())
        .unwrap();
    let mut committed = vec![0_u8; 32 * 1024 * 1024];
    committed.fill(0xa5);
    std::hint::black_box(&committed);
    let after = collector.snapshot().unwrap();
    let own_after = after
        .processes
        .iter()
        .find(|p| p.identity.pid == std::process::id())
        .unwrap();
    assert_eq!(own_before.identity, own_after.identity);
    let growth = own_after
        .memory_bytes
        .value
        .unwrap()
        .saturating_sub(own_before.memory_bytes.value.unwrap());
    assert!(
        growth >= 16 * 1024 * 1024,
        "committing 32 MiB must produce a material RSS increase, observed {growth} bytes"
    );
    std::hint::black_box(committed);
}
