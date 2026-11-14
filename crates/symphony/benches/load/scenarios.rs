//! Orchestrator scenarios: dispatch bursts, arrivals between polls, the event-rate ramp and the
//! soak. Each returns its results as JSON and prints a short table.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::watch;

use crate::Res;
use crate::probes::{GenTimeline, RssProbe, SnapshotProbe, SnapshotStats, SseProbe, SseStats};
use crate::rig::{Rig, RigConfig, StoreKind};
use crate::stats::{Summary, fmt_ms, mib, ms, round3, slope};
use crate::world::{Plan, RunLength};

/// A poll interval long enough that only an explicit refresh starts a poll cycle.
const NO_TIMER_POLLS_MS: u64 = 3_600_000;
/// Spare slots kept free for arriving issues in [`arrivals`].
const SPARE_SLOTS: u32 = 12;
/// Issues that arrive per poll interval in [`arrivals`] (fewer than the spare slots).
const ARRIVALS_PER_INTERVAL: u64 = 10;
/// A ramp step ends early at this backlog, so an overloaded step cannot exhaust memory.
const MAX_BACKLOG: u64 = 1_000_000;

fn started(rig: &Rig) -> u64 {
    rig.world.metrics.started.load(Ordering::Relaxed)
}

fn emitted(rig: &Rig) -> u64 {
    rig.world.metrics.emitted.load(Ordering::Relaxed)
}

// ----- 1a. dispatch burst ------------------------------------------------------------------------

/// `agents` issues become eligible at once with `agents` free slots, and a refresh is requested
/// (what `POST /api/v1/refresh` does). Measures eligible -> worker start for every issue, and
/// snapshot latency while the poll cycle runs.
pub async fn dispatch_burst(
    agents: u32,
    tracker_latency_ms: u64,
    rounds: u32,
    store: StoreKind,
) -> Res<Value> {
    let latency = Duration::from_millis(tracker_latency_ms);
    let mut all = Vec::new();
    let (mut first, mut last, mut per_dispatch) = (Vec::new(), Vec::new(), Vec::new());
    let mut snapshots = Vec::new();
    let mut tracker_calls = 0;
    for _ in 0..rounds {
        let rig = Rig::start(RigConfig {
            agents,
            poll_interval_ms: NO_TIMER_POLLS_MS,
            store,
            tracker_latency: latency,
            serve: false,
            plan: Plan::new(0.0, RunLength::Forever, false),
        })
        .await?;
        let probe = SnapshotProbe::start(rig.handle.clone(), Duration::from_millis(20));
        let calls = rig.tracker.calls.load(Ordering::Relaxed);
        let eligible = rig.world.add_issues(u64::from(agents));
        rig.handle.request_refresh().await?;
        let limit = Duration::from_secs(120) + latency * (2 * agents + 10);
        rig.wait_started(u64::from(agents), limit).await?;
        let done = Instant::now();
        tracker_calls = rig.tracker.calls.load(Ordering::Relaxed) - calls;
        let latencies = rig.world.metrics.dispatch_latencies();
        let earliest = latencies.iter().min().copied().unwrap_or_default();
        let latest = latencies.iter().max().copied().unwrap_or_default();
        first.push(earliest);
        last.push(latest);
        // Time per dispatch once the cycle has started dispatching.
        per_dispatch.push((latest - earliest).div_f64(f64::from(agents.max(2) - 1)));
        all.extend(latencies);
        snapshots.extend(probe.window(eligible, done));
        drop(probe);
        rig.stop().await?;
    }
    let latency_summary = Summary::of(&all);
    let first = Summary::of(&first);
    let fill = Summary::of(&last);
    let per_dispatch = Summary::of(&per_dispatch);
    let snapshot = SnapshotStats::of(&snapshots);
    println!(
        "  burst    agents={agents:<4} tracker={tracker_latency_ms:>3}ms  first start {:>8} ms  \
         p50 {:>8}  last {:>8}  per dispatch {:>7} ms  snapshot max {:>8} ms",
        fmt_ms(first.p50),
        fmt_ms(latency_summary.p50),
        fmt_ms(fill.p50),
        fmt_ms(per_dispatch.p50),
        fmt_ms(snapshot.latency.max),
    );
    Ok(json!({
        "agents": agents,
        "tracker_latency_ms": tracker_latency_ms,
        "rounds": rounds,
        "eligible_to_start": latency_summary.json(),
        "first_start": first.json(),
        "all_started": fill.json(),
        "per_dispatch_ms": round3(per_dispatch.p50),
        "tracker_calls_per_round": tracker_calls,
        "snapshot_during_burst": snapshot.json(),
    }))
}

// ----- 1b. arrivals between polls ----------------------------------------------------------------

/// `agents` workers run and emit events while issues arrive at random times into free slots and
/// nobody requests a refresh: eligible -> worker start is the wait for the next poll tick plus
/// the poll cycle.
pub async fn arrivals(
    agents: u32,
    poll_interval_ms: u64,
    rate: f64,
    intervals: u32,
    store: StoreKind,
) -> Res<Value> {
    let rig = Rig::start(RigConfig {
        agents: agents + SPARE_SLOTS,
        poll_interval_ms,
        store,
        tracker_latency: Duration::ZERO,
        serve: false,
        // A short run ends within a third of the interval, so its slot is free again by the
        // poll after the one that started it.
        plan: Plan::new(rate, RunLength::Forever, false)
            .with_short_run(Duration::from_millis(poll_interval_ms) / 3),
    })
    .await?;
    rig.world.add_issues(u64::from(agents));
    rig.handle.request_refresh().await?;
    rig.wait_started(u64::from(agents), Duration::from_secs(120))
        .await?;
    let base = rig.world.metrics.dispatch_latencies().len();

    // Arrivals are spread evenly over the span with a pseudo-random offset each.
    let arrivals = u64::from(intervals) * ARRIVALS_PER_INTERVAL;
    let span = Duration::from_millis(poll_interval_ms) * intervals;
    let step = span.div_f64(arrivals as f64);
    let begin = Instant::now();
    for index in 0..arrivals {
        let jitter = ((index * 7_919) % 1_000) as f64 / 1_000.0;
        let due = begin + step.mul_f64(index as f64 + jitter);
        tokio::time::sleep(due.saturating_duration_since(Instant::now())).await;
        rig.world.add_short_issue();
    }
    rig.wait_started(
        u64::from(agents) + arrivals,
        Duration::from_millis(2 * poll_interval_ms) + Duration::from_secs(30),
    )
    .await?;
    let latencies: Vec<Duration> = rig.world.metrics.dispatch_latencies()[base..].to_vec();
    rig.stop().await?;
    let summary = Summary::of(&latencies);
    println!(
        "  arrivals agents={agents:<4} poll={poll_interval_ms}ms  n={}  eligible->start mean {} ms  \
         p50 {}  max {}",
        summary.n,
        fmt_ms(summary.mean),
        fmt_ms(summary.p50),
        fmt_ms(summary.max),
    );
    Ok(json!({
        "agents": agents,
        "poll_interval_ms": poll_interval_ms,
        "events_per_agent_per_sec": rate,
        "arrivals": arrivals,
        "eligible_to_start": summary.json(),
    }))
}

// ----- 2. event-rate ramp ------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Mark {
    at: Instant,
    emitted: u64,
    generation: u64,
    persisted: u64,
}

async fn mark(rig: &Rig, generation: &watch::Receiver<u64>) -> Res<Mark> {
    let persisted = rig.persisted_events().await?;
    Ok(Mark {
        at: Instant::now(),
        emitted: emitted(rig),
        generation: *generation.borrow(),
        persisted,
    })
}

/// `agents` workers emit events at a total rate that steps up through `rates`. The orchestrator
/// bumps its generation counter once per integrated event, so offered and integrated rates are
/// both exact counts; their running difference is the backlog in the per-run event channels.
pub async fn event_ramp(
    agents: u32,
    rates: &[u64],
    step_secs: u64,
    store: StoreKind,
) -> Res<Value> {
    let rig = Rig::start(RigConfig {
        agents,
        poll_interval_ms: 30_000,
        store,
        tracker_latency: Duration::ZERO,
        serve: true,
        plan: Plan::new(0.0, RunLength::Forever, false),
    })
    .await?;
    rig.world.add_issues(u64::from(agents));
    rig.handle.request_refresh().await?;
    rig.wait_started(u64::from(agents), Duration::from_secs(120))
        .await?;

    let generation = rig.handle.subscribe();
    let timeline = GenTimeline::start(rig.handle.subscribe());
    let snapshots = SnapshotProbe::start(rig.handle.clone(), Duration::from_millis(100));
    let sse = SseProbe::start(rig.events_url()?, &timeline);
    let rss = RssProbe::start(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let idle_rss = rss.last().unwrap_or(0);
    let origin = mark(&rig, &generation).await?;

    println!(
        "  ramp     agents={agents}: target/s  offered/s  integrated/s  backlog  queue-delay p50/max ms  \
         snapshot p50/p99/max ms  sse gap p50/max ms  persisted/s  rss MiB"
    );
    let mut steps = Vec::new();
    let mut sustained = 0.0_f64;
    let mut capacity = 0.0_f64;
    let mut peak_backlog = 0u64;
    for &target in rates {
        rig.world.plan.set_rate(target as f64 / f64::from(agents));
        tokio::time::sleep(Duration::from_secs(2)).await;
        let backlog_of = |m: &Mark| {
            (m.emitted - origin.emitted).saturating_sub(m.generation - origin.generation)
        };
        let a = mark(&rig, &generation).await?;
        let step_end = a.at + Duration::from_secs(step_secs);
        // Watch the backlog with the counters alone (no store query) until the step is over.
        let mut backlog = backlog_of(&a);
        while Instant::now() < step_end && backlog < MAX_BACKLOG {
            tokio::time::sleep(Duration::from_millis(250)).await;
            backlog = (emitted(&rig) - origin.emitted)
                .saturating_sub(*generation.borrow() - origin.generation);
        }
        let b = mark(&rig, &generation).await?;
        let seconds = b.at.duration_since(a.at).as_secs_f64();
        let offered = (b.emitted - a.emitted) as f64 / seconds;
        let integrated = (b.generation - a.generation) as f64 / seconds;
        let persisted = (b.persisted - a.persisted) as f64 / seconds;
        let backlog = backlog_of(&b);
        let growth = (backlog as f64 - backlog_of(&a) as f64) / seconds;
        peak_backlog = peak_backlog.max(backlog);
        let snapshot = SnapshotStats::of(&snapshots.window(a.at, b.at));
        let frames = SseStats::of(&sse.window(a.at, b.at));
        let rss_now = rss.last().unwrap_or(0);
        // Keeping up: the backlog does not grow by more than 2 % of the offered rate.
        let keeps_up = growth < 0.02 * offered;
        if keeps_up {
            sustained = sustained.max(integrated);
        }
        capacity = capacity.max(integrated);
        println!(
            "    {target:>9}  {offered:>9.0}  {integrated:>12.0}  {backlog:>7}  {:>10} / {:<8}  {:<24}  {:>6} / {:<7}  {persisted:>9.0}  {:>6.0}{}",
            fmt_ms(snapshot.queue_delay.p50),
            fmt_ms(snapshot.queue_delay.max),
            snapshot.latency.brief(),
            fmt_ms(frames.gap.p50),
            fmt_ms(frames.gap.max),
            mib(rss_now),
            if keeps_up { "" } else { "  <- backlog grows" },
        );
        steps.push(json!({
            "target_per_sec": target,
            "offered_per_sec": offered.round(),
            "integrated_per_sec": integrated.round(),
            "persisted_per_sec": persisted.round(),
            "backlog_events": backlog,
            "backlog_growth_per_sec": growth.round(),
            "keeps_up": keeps_up,
            "snapshot": snapshot.json(),
            "sse": frames.json(),
            "rss_bytes": rss_now,
        }));
        if !keeps_up {
            break;
        }
    }

    // Stop producing and time how long the orchestrator needs to work the backlog off.
    rig.world.plan.set_rate(0.0);
    let stopped = Instant::now();
    let peak_rss = rss
        .window(origin.at, Instant::now())
        .iter()
        .map(|(_, bytes)| *bytes)
        .max()
        .unwrap_or(0);
    let mut previous = *generation.borrow();
    let mut quiet = 0;
    while quiet < 5 && stopped.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let current = *generation.borrow();
        quiet = if current == previous { quiet + 1 } else { 0 };
        previous = current;
    }
    let drain = stopped.elapsed().saturating_sub(Duration::from_millis(500));
    let end = mark(&rig, &generation).await?;
    let total_emitted = end.emitted - origin.emitted;
    let sse_error = sse.error();
    drop((snapshots, sse, rss, timeline));
    let counts = rig.stop().await?;
    let bytes_per_backlog_event = if peak_backlog > 10_000 {
        (peak_rss.saturating_sub(idle_rss)) as f64 / peak_backlog as f64
    } else {
        0.0
    };
    println!(
        "    sustained {sustained:.0}/s, peak integrated {capacity:.0}/s, peak backlog {peak_backlog} events, \
         drained in {:.1} s, peak rss {:.0} MiB (idle {:.0}); store: {} of {total_emitted} events persisted",
        drain.as_secs_f64(),
        mib(peak_rss),
        mib(idle_rss),
        counts.events,
    );
    Ok(json!({
        "agents": agents,
        "step_secs": step_secs,
        "steps": steps,
        "sustained_per_sec": sustained.round(),
        "peak_integrated_per_sec": capacity.round(),
        "peak_backlog_events": peak_backlog,
        "drain_secs": round3(drain.as_secs_f64()),
        "idle_rss_bytes": idle_rss,
        "peak_rss_bytes": peak_rss,
        "rss_bytes_per_backlog_event": bytes_per_backlog_event.round(),
        "emitted_events": total_emitted,
        "store": counts.json(),
        "sse_error": sse_error,
    }))
}

// ----- 2/4. soak ---------------------------------------------------------------------------------

/// Parameters of [`soak`].
#[derive(Debug, Clone, Copy)]
pub struct SoakConfig {
    pub agents: u32,
    pub secs: u64,
    /// Events per second of each agent.
    pub rate: f64,
    /// Mean run length; runs last between half and one and a half times this.
    pub run_secs: u64,
    pub poll_interval_ms: u64,
    /// Delay added to every tracker call.
    pub tracker_latency: Duration,
    pub store: StoreKind,
}

/// Groups worker starts into dispatch waves (starts less than a second apart) and returns each
/// wave's size and duration.
fn waves(mut starts: Vec<Instant>) -> Vec<(usize, Duration)> {
    starts.sort();
    let mut waves = Vec::new();
    let mut iter = starts.into_iter();
    let Some(mut first) = iter.next() else {
        return waves;
    };
    let (mut last, mut size) = (first, 1usize);
    for at in iter {
        if at.duration_since(last) > Duration::from_secs(1) {
            waves.push((size, last.duration_since(first)));
            first = at;
            size = 0;
        }
        last = at;
        size += 1;
    }
    waves.push((size, last.duration_since(first)));
    waves
}

/// Steady state at capacity: `agents` slots, a standing backlog of eligible issues, runs that end
/// and hand off, the default poll timer, a dashboard client on the SSE stream and a snapshot
/// every 100 ms.
pub async fn soak(config: SoakConfig) -> Res<Value> {
    let SoakConfig {
        agents,
        secs,
        rate,
        run_secs,
        poll_interval_ms,
        tracker_latency,
        store,
    } = config;
    let run = Duration::from_secs(run_secs);
    let rig = Rig::start(RigConfig {
        agents,
        poll_interval_ms,
        store,
        tracker_latency,
        serve: true,
        plan: Plan::new(rate, RunLength::Uniform(run / 2, run * 3 / 2), true),
    })
    .await?;
    let generation = rig.handle.subscribe();
    let timeline = GenTimeline::start(rig.handle.subscribe());
    let snapshots = SnapshotProbe::start(rig.handle.clone(), Duration::from_millis(100));
    let sse = SseProbe::start(rig.events_url()?, &timeline);
    let rss = RssProbe::start(Duration::from_secs(5));

    // One issue per slot; every start adds a new one, so as many again are always waiting and
    // every freed slot has work.
    let begin = rig.world.add_issues(u64::from(agents));
    rig.handle.request_refresh().await?;
    let warmup = Duration::from_secs((secs / 5).min(60));
    let end = begin + Duration::from_secs(secs);
    println!(
        "  soak     agents={agents} rate={rate}/s/agent run={run_secs}s poll={poll_interval_ms}ms \
         tracker={}ms: \
         t s  running  offered/s  integrated/s  queue-delay max ms  snapshot p99 ms  rss MiB",
        tracker_latency.as_millis()
    );
    let mut warm: Option<Mark> = None;
    let mut previous = mark(&rig, &generation).await?;
    let mut series = Vec::new();
    while Instant::now() < end {
        let next = (previous.at + Duration::from_secs(30)).min(end);
        tokio::time::sleep(next.saturating_duration_since(Instant::now())).await;
        let now = mark(&rig, &generation).await?;
        if warm.is_none() && now.at.duration_since(begin) >= warmup {
            warm = Some(now);
        }
        let seconds = now.at.duration_since(previous.at).as_secs_f64();
        let window = SnapshotStats::of(&snapshots.window(previous.at, now.at));
        let rss_now = rss.last().unwrap_or(0);
        println!(
            "    {:>5.0}  {:>7.0}  {:>9.0}  {:>12.0}  {:>18}  {:>15}  {:>7.1}",
            now.at.duration_since(begin).as_secs_f64(),
            window.mean_running,
            (now.emitted - previous.emitted) as f64 / seconds,
            (now.generation - previous.generation) as f64 / seconds,
            fmt_ms(window.queue_delay.max),
            fmt_ms(window.latency.p99),
            mib(rss_now),
        );
        series.push(json!({
            "t_secs": now.at.duration_since(begin).as_secs_f64().round(),
            "mean_running": round3(window.mean_running),
            "rss_bytes": rss_now,
            "snapshot_p99_ms": round3(window.latency.p99),
            "queue_delay_max_ms": round3(window.queue_delay.max),
        }));
        previous = now;
    }
    let finish = previous;
    let warm = warm.unwrap_or(finish);
    let seconds = finish.at.duration_since(warm.at).as_secs_f64().max(1e-9);

    let memory: Vec<(f64, f64)> = rss
        .window(warm.at, finish.at)
        .iter()
        .map(|(at, bytes)| (at.duration_since(warm.at).as_secs_f64() / 60.0, mib(*bytes)))
        .collect();
    let rss_first = memory.first().map_or(0.0, |p| p.1);
    let rss_last = memory.last().map_or(0.0, |p| p.1);
    let rss_max = memory.iter().map(|p| p.1).fold(0.0, f64::max);
    let rss_slope = slope(&memory);

    let snapshot = SnapshotStats::of(&snapshots.window(warm.at, finish.at));
    let frames = SseStats::of(&sse.window(warm.at, finish.at));
    let slot_idle = Summary::of(&rig.world.metrics.slot_idle());
    let wave_list = waves(rig.world.metrics.starts());
    // The first wave fills the empty system; later ones refill freed slots.
    let refills: Vec<(usize, Duration)> = wave_list.iter().skip(1).copied().collect();
    let wave_sizes: Vec<f64> = refills.iter().map(|(size, _)| *size as f64).collect();
    let wave_times: Vec<Duration> = refills.iter().map(|(_, time)| *time).collect();
    let wave_time = Summary::of(&wave_times);
    let wave_size = Summary::of_ms(&wave_sizes);
    let runs_started = started(&rig);
    let tracker_calls = rig.tracker.calls.load(Ordering::Relaxed);
    let sse_error = sse.error();
    drop((snapshots, sse, rss, timeline));
    let counts = rig.stop().await?;
    let offered = (finish.emitted - warm.emitted) as f64 / seconds;
    let integrated = (finish.generation - warm.generation) as f64 / seconds;
    println!(
        "    after warm-up: offered {offered:.0}/s, integrated {integrated:.0}/s; rss {rss_first:.1} -> {rss_last:.1} MiB \
         (max {rss_max:.1}, slope {rss_slope:+.2} MiB/min); occupancy {:.0} %; {:.0} tracker calls/min; \
         freed slot idle mean {} ms, max {} ms; \
         refill wave {:.0} starts in {} ms (max {}); snapshot p50/p99/max {} ms; sse gap p50/p99/max {} ms, \
         delivery p50/p99/max {} ms, frame {:.0} KiB",
        100.0 * snapshot.mean_running / f64::from(agents),
        tracker_calls as f64 * 60.0 / secs as f64,
        fmt_ms(slot_idle.mean),
        fmt_ms(slot_idle.max),
        wave_size.mean,
        fmt_ms(wave_time.p50),
        fmt_ms(wave_time.max),
        snapshot.latency.brief(),
        frames.gap.brief(),
        frames.delivery.brief(),
        frames.mean_bytes / 1024.0,
    );
    Ok(json!({
        "agents": agents,
        "secs": secs,
        "warmup_secs": warmup.as_secs(),
        "events_per_agent_per_sec": rate,
        "mean_run_secs": run_secs,
        "poll_interval_ms": poll_interval_ms,
        "tracker_latency_ms": tracker_latency.as_millis(),
        "offered_per_sec": offered.round(),
        "integrated_per_sec": integrated.round(),
        "runs_started": runs_started,
        "tracker_calls_per_min": round3(tracker_calls as f64 * 60.0 / secs as f64),
        "occupancy": round3(snapshot.mean_running / f64::from(agents)),
        "rss_mib": {
            "after_warmup": round3(rss_first),
            "end": round3(rss_last),
            "max": round3(rss_max),
            "slope_per_min": round3(rss_slope),
        },
        "freed_slot_idle": slot_idle.json(),
        "refill_wave": {
            "waves": refills.len(),
            "mean_starts": round3(wave_size.mean),
            "max_starts": wave_size.max,
            "duration": wave_time.json(),
        },
        "snapshot": snapshot.json(),
        "sse": frames.json(),
        "sse_error": sse_error,
        "store": counts.json(),
        "series": series,
        "seconds_measured": round3(ms(finish.at.duration_since(warm.at)) / 1_000.0),
    }))
}
