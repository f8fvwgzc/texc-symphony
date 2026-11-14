//! The store on its own: how many writes per second the single database thread takes from
//! `writers` concurrent submitters, what a read and a lease renewal cost while they write, and
//! when a submission is refused as `Busy`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};
use symphony_store::{NewRun, RunId, RunQuery, STORE_QUEUE_CAPACITY, Store, StoreError};
use tokio::task::JoinSet;

use crate::Res;
use crate::stats::{Summary, fmt_ms, round3};

/// Lease owner used by the benchmark.
const OWNER: &str = "load-harness";
/// How long the fire-and-forget flood lasts.
const FLOOD: Duration = Duration::from_secs(3);

/// Which database the store talks to.
#[derive(Debug, Clone)]
pub enum Backend {
    /// A SQLite file in a temporary directory.
    Sqlite,
    /// The PostgreSQL database at this URL.
    Postgres(String),
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres(_) => "postgres",
        }
    }
}

/// An event of the size the recorder writes for an `item/completed` notification.
fn event_payload(seq: u64) -> Value {
    json!({
        "event": "notification",
        "timestamp": "2026-11-10T09:00:00.000Z",
        "codex_app_server_pid": "4242",
        "payload": {
            "method": "item/completed",
            "params": {
                "threadId": "thread-0000002a",
                "turnId": "turn-1",
                "item": {
                    "type": "commandExecution",
                    "id": format!("item-{seq}"),
                    "command": "cargo test -p symphony-runtime --lib orchestrator",
                    "cwd": "/workspaces/LOAD/repo",
                    "aggregatedOutput": "running 42 tests\ntest orchestrator::tests::dispatches_in_priority_order ... ok\ntest orchestrator::tests::retries_with_backoff ... ok\n\ntest result: ok. 42 passed; 0 failed; 0 ignored; finished in 1.84s\n",
                    "exitCode": 0,
                    "durationMs": 1_840,
                    "status": "completed",
                },
            },
        },
    })
}

fn append(store: &Store, run: RunId, seq: u64) -> symphony_store::Pending<i64> {
    store.append_event(
        run,
        "notification",
        Some("item/completed".to_owned()),
        Some(event_payload(seq)),
    )
}

/// Runs `probe` every `period` until `stop`; returns the latencies of the calls that succeeded
/// and the number that failed.
async fn sample<F, Fut>(stop: Arc<AtomicBool>, period: Duration, probe: F) -> (Vec<Duration>, u64)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let mut latencies = Vec::new();
    let mut failed = 0;
    while !stop.load(Ordering::Relaxed) {
        let at = Instant::now();
        if probe().await {
            latencies.push(at.elapsed());
        } else {
            failed += 1;
        }
        tokio::time::sleep(period).await;
    }
    (latencies, failed)
}

/// Benchmarks `backend` with `writers` concurrent writers for `secs` seconds.
pub async fn store_bench(writers: u32, secs: u64, backend: &Backend) -> Res<Value> {
    let dir = tempfile::tempdir()?;
    let store = match backend {
        Backend::Sqlite => Store::open(dir.path().join("symphony.db"))?,
        Backend::Postgres(url) => Store::open_postgres(url)?,
    };
    let mut runs = Vec::new();
    let mut keys = Vec::new();
    for index in 0..writers {
        let issue_id = format!("store-bench-{}-{index}", std::process::id());
        let run = store
            .start_run(NewRun {
                issue_id: issue_id.clone(),
                issue_identifier: format!("BENCH-{index}"),
                issue_title: Some("store benchmark".to_owned()),
                attempt: 0,
                worker_host: None,
                workspace_path: None,
                started_at: Some(Utc::now()),
            })
            .await?;
        store
            .acquire_lease(issue_id.clone(), OWNER, Duration::from_secs(60))
            .await?;
        runs.push(run);
        keys.push(issue_id);
    }

    // ----- closed loop: every writer awaits its write before the next, like a run's recorder.
    let stop = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicU64::new(0));
    let mut tasks = JoinSet::new();
    for run in runs.clone() {
        let (store, stop, busy) = (store.clone(), Arc::clone(&stop), Arc::clone(&busy));
        tasks.spawn(async move {
            let mut latencies = Vec::new();
            let mut seq = 0;
            while !stop.load(Ordering::Relaxed) {
                seq += 1;
                let at = Instant::now();
                match append(&store, run, seq).await {
                    Ok(_) => latencies.push(at.elapsed()),
                    Err(StoreError::Busy) => {
                        busy.fetch_add(1, Ordering::Relaxed);
                        tokio::task::yield_now().await;
                    }
                    Err(_) => break,
                }
            }
            latencies
        });
    }
    let reader = {
        let store = store.clone();
        tokio::spawn(sample(
            Arc::clone(&stop),
            Duration::from_millis(50),
            move || {
                let store = store.clone();
                async move { store.list_runs(RunQuery::default()).await.is_ok() }
            },
        ))
    };
    let renewer = {
        let (store, keys) = (store.clone(), keys.clone());
        tokio::spawn(sample(
            Arc::clone(&stop),
            Duration::from_millis(250),
            move || {
                let (store, keys) = (store.clone(), keys.clone());
                async move {
                    store
                        .renew_leases(OWNER, keys, Duration::from_secs(60))
                        .await
                        .is_ok()
                }
            },
        ))
    };
    let begin = Instant::now();
    tokio::time::sleep(Duration::from_secs(secs)).await;
    stop.store(true, Ordering::Relaxed);
    let elapsed = begin.elapsed().as_secs_f64();
    let mut write_latencies = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        write_latencies.extend(joined?);
    }
    let read = Summary::of(&reader.await?.0);
    let renew = Summary::of(&renewer.await?.0);
    let write = Summary::of(&write_latencies);
    let writes_per_sec = write_latencies.len() as f64 / elapsed;
    let closed_busy = busy.load(Ordering::Relaxed);

    // ----- open loop: fire-and-forget submissions as fast as one task can make them.
    store.flush().await?;
    let flood_stop = Arc::new(AtomicBool::new(false));
    let flood_reader = {
        let (store, run) = (store.clone(), runs[0]);
        tokio::spawn(sample(
            Arc::clone(&flood_stop),
            Duration::from_millis(20),
            move || {
                let store = store.clone();
                async move { store.get_run(run).await.is_ok() }
            },
        ))
    };
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let (mut accepted, mut refused) = (0u64, 0u64);
    let flood_begin = Instant::now();
    let mut seq = 0u64;
    while flood_begin.elapsed() < FLOOD {
        for _ in 0..64 {
            seq += 1;
            let run = runs[usize::try_from(seq).unwrap_or(0) % runs.len()];
            let mut pending = append(&store, run, seq);
            // A refused submission is ready at once; an accepted one is dropped (fire and forget).
            match Pin::new(&mut pending).poll(&mut cx) {
                Poll::Ready(Err(StoreError::Busy)) => refused += 1,
                _ => accepted += 1,
            }
        }
        // Let the reader and the runtime's timers in.
        tokio::task::yield_now().await;
    }
    let flood_secs = flood_begin.elapsed().as_secs_f64();
    let drain_begin = Instant::now();
    // The queue may still be full: retry until the flush itself is accepted.
    while matches!(store.flush().await, Err(StoreError::Busy)) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let drain = drain_begin.elapsed();
    flood_stop.store(true, Ordering::Relaxed);
    let (flood_reads, flood_reads_refused) = flood_reader.await?;
    let flood_read = Summary::of(&flood_reads);
    let offered = (accepted + refused) as f64 / flood_secs;

    println!(
        "  store    {} writers={writers:<4} {writes_per_sec:>8.0} writes/s  write p50/p99/max {} ms  \
         read {} ms  renew({writers} leases) {} ms  busy {closed_busy} | flood: offered {offered:.0}/s, \
         accepted {:.0}/s, refused {:.1} %, read p50/max {} / {} ms ({flood_reads_refused} reads refused), \
         queue drained in {} ms",
        backend.name(),
        write.brief(),
        read.brief(),
        renew.brief(),
        accepted as f64 / flood_secs,
        100.0 * refused as f64 / (accepted + refused).max(1) as f64,
        fmt_ms(flood_read.p50),
        fmt_ms(flood_read.max),
        fmt_ms(drain.as_secs_f64() * 1_000.0),
    );
    Ok(json!({
        "backend": backend.name(),
        "writers": writers,
        "secs": secs,
        "queue_capacity": STORE_QUEUE_CAPACITY,
        "closed_loop": {
            "writes_per_sec": writes_per_sec.round(),
            "write": write.json(),
            "list_runs": read.json(),
            "renew_leases": renew.json(),
            "busy": closed_busy,
        },
        "flood": {
            "secs": round3(flood_secs),
            "offered_per_sec": offered.round(),
            "accepted_per_sec": (accepted as f64 / flood_secs).round(),
            "refused": refused,
            "refused_fraction": round3(refused as f64 / (accepted + refused).max(1) as f64),
            "get_run": flood_read.json(),
            "get_run_refused": flood_reads_refused,
            "drain_ms": round3(drain.as_secs_f64() * 1_000.0),
        },
    }))
}
