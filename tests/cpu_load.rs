#![cfg(target_os = "macos")]
//! A bounded, owned CPU fixture. This test targets only its own process and never
//! accepts a PID, spawns a shell, or sends a signal to another process.
use bree_cli::collect::Collector;
use bree_cli::model::Validity;
use std::time::{Duration, Instant};

#[test]
fn bounded_single_busy_thread_is_measured_near_one_core() {
    // This integration-test executable has only one test, avoiding unrelated
    // concurrent tests in the process whose cumulative CPU time is measured.
    let mut collector = Collector::new().expect("macOS collector initializes");
    let baseline = collector.snapshot().expect("baseline snapshot");
    let own = baseline
        .processes
        .iter()
        .find(|p| p.identity.pid == std::process::id())
        .expect("fixture process appears in baseline");
    let identity = own.identity.clone();
    assert_eq!(identity.status, Validity::Ok);
    assert_eq!(
        own.cpu_one_core_percent.value, None,
        "first sample establishes only a baseline"
    );

    let fixture = std::thread::spawn(|| {
        let started = Instant::now();
        let mut value = 0x1234_5678_90ab_cdef_u64;
        // Time bounded independently of the observer: no cancellation or kill
        // is needed even if sampling fails.
        while started.elapsed() < Duration::from_secs(2) {
            for _ in 0..1024 {
                value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(value);
            }
        }
        std::hint::black_box(value)
    });

    let mut observations = Vec::new();
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(600));
        observations.push(collector.snapshot());
    }
    fixture
        .join()
        .expect("owned fixture naturally finishes and is joined");

    let mut percentages = Vec::new();
    for observation in observations {
        let snapshot = observation.expect("CPU observation snapshot");
        let measured = snapshot
            .processes
            .iter()
            .find(|p| p.identity.pid == identity.pid)
            .expect("same fixture process remains visible");
        assert_eq!(
            measured.identity, identity,
            "CPU differences must stay in the same precise instance"
        );
        assert_eq!(measured.cpu_one_core_percent.status, Validity::Ok);
        percentages.push(
            measured
                .cpu_one_core_percent
                .value
                .expect("real CPU time difference"),
        );
    }
    println!("owned single-core fixture CPU samples: {percentages:?}%");
    // Scheduler load and sampling overhead may move this away from exactly
    // 100%. The wide bounds still decisively reject ARM Mach ticks being
    // mistaken for nanoseconds (~2.4%) or converted twice (~4166.7%).
    for percentage in percentages {
        assert!(
            (50.0..=160.0).contains(&percentage),
            "single busy thread should consume about one core; observed {percentage}%"
        );
    }
}
