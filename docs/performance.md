# Performance

How the orchestrator and the store behave under load, measured with the load harness in
[`crates/symphony/benches/load`](../crates/symphony/benches/load). Every number on this page
comes from one run of the command below on 2026-11-10, at commit `f877d3a`, on one laptop
([Machine](#machine)). They describe that run: they are evidence, not guarantees for other
hardware, for a real tracker or for real agents.

```bash
cargo bench -p symphony --bench load
```

The run takes about 50 minutes. Its raw results are in [`performance/results`](performance/results).

## Summary

| Measurement | 10 agents | 100 agents | 500 agents |
|---|---|---|---|
| Issue eligible → worker started, refresh requested, no tracker delay: first / last of a full burst | 23 ms / 24 ms | 23 ms / 42 ms | 23 ms / 153 ms |
| Same with 50 ms per tracker call: last worker of the burst | 0.6 s | 5.3 s | 26.2 s |
| Issue eligible → worker started, no refresh (30 s poll timer): mean / max | 15.0 s / 30.0 s | 15.0 s / 30.0 s | 15.0 s / 30.0 s |
| Freed slot stays empty while work waits: mean / max | 15.5 s / 29.3 s | 14.8 s / 30.0 s | 15.2 s / 30.1 s |
| Agent events integrated per second before the queue grows | 40,000 | 35,000 | 25,000 |
| Resident memory during a 10-minute soak (10 events/s per agent): after 1 min → end | 18.0 → 21.2 MiB | 27.8 → 26.1 MiB | 51.6 → 50.6 MiB |
| SQLite store, writes per second with that many concurrent writers | 26,900 | 20,500 | 16,100 |
| Store queue answered `Busy` during the orchestrator runs | never | never | never |
| `RuntimeHandle::snapshot`, p50 / p99 / max during the soak | 0.35 / 0.82 / 5.8 ms | 0.29 / 2.1 / 28 ms | 0.66 / 6.8 / 91 ms |
| SSE frame: gap between frames p50, delivery p99, size | 202 ms, 65 ms, 5 KiB | 202 ms, 61 ms, 41 KiB | 203 ms, 24 ms, 205 KiB |

What these numbers support, and what they do not:

- In the ten-minute soak with 500 slots (400 agents running on average, 4,000 events/s) memory
  stayed flat at 51 MiB, `snapshot()` took 6.8 ms at p99 and no history was lost. The
  orchestrator's ceiling on this machine is 25,000 events/s at 500 agents.
- "Fast" has a floor set by design, not by CPU: nothing but the poll timer (or an explicit
  refresh) starts work, so a new issue waits 15 s on average with the default 30 s interval, and
  a freed slot stays empty just as long ([bottleneck 1](#bottlenecks-ranked)).
- The largest CPU cost is avoidable: `WORKFLOW.md` is read from disk and hashed after every
  agent event ([bottleneck 3](#bottlenecks-ranked)). A one-line experiment doubled the ceiling at
  500 agents (25,000 to 50,000 events/s) and a second one raised it to 400,000
  ([experiments](#throwaway-experiments)).
- Past its ceiling the orchestrator does not slow producers down or shed load: event queues are
  unbounded, and memory grows by 1.5 to 3.3 KB per queued event for as long as the overload
  lasts.
- Nothing here measured a real tracker, a real agent, PostgreSQL, several replicas, or Linux
  ([Not measured](#not-measured)).

## Method

### What is real and what is fake

The harness runs, in one process, the same pieces the `symphony` binary assembles: a
`WORKFLOW.md` with `tracker.kind: memory` (front matter of the harness, prompt of this
repository's own `WORKFLOW.md`, 20 KB in all), `WorkflowStore`, the runtime (`Runtime::start`:
supervisor, orchestrator actor, recorders), a SQLite `Store` on a file in the system temporary
directory, and the HTTP server on a loopback port with `RuntimeControlPlane`. Two things are
replaced:

- **The worker.** `RuntimeOptions::with_worker_factory` installs a fake worker
  ([`world.rs`](../crates/symphony/benches/load/world.rs)). It reports its workspace, emits a
  `session_started` event, then emits Codex-style notifications at the configured rate, and
  either runs until cancelled or ends after its run length with `turn/completed`, moving its
  issue to `Human Review` like an agent handing off. No process is spawned and no model is
  called. Of every 20 events, 17 are `item/agentMessage/delta` (the recorder keeps at most one
  per second and run), 1 is `item/started`, 1 is `item/completed` with command output (both are
  recorded every time), and 1 is `thread/tokenUsage/updated` with growing totals.
- **The issue list.** The memory tracker serves a list the harness edits. An optional fixed
  delay per tracker call stands in for a remote API.

Each scenario runs once per scale, in its own process, so no run inherits memory from another.

| Scenario | What it does | What it measures |
|---|---|---|
| `dispatch` (burst) | N issues become eligible at once with N free slots, then `request_refresh()` (what `POST /api/v1/refresh` does). Five rounds each without and with a 50 ms tracker delay. | Eligible → worker started for every issue; `snapshot()` latency during the poll cycle. |
| `dispatch` (arrivals) | N workers run at 10 events/s; 30 issues arrive at spread-out times into 12 spare slots over three poll intervals; nobody requests a refresh. | Eligible → worker started when only the poll timer starts work. |
| `events` | N workers run until cancelled; their total event rate steps up (1,000 to 400,000 events/s, 8 s per step after 2 s of settling) until the backlog grows. | Offered and integrated events per second, backlog, queue delay, memory; snapshot and SSE latency at each step. |
| `store` | The store alone: N writers each await one `append_event` after another for 10 s, while `list_runs` and a renewal of N leases are timed; then one task submits fire-and-forget appends as fast as it can for 3 s. | Writes per second, write/read/renew latency, how often a submission is refused as `Busy`. |
| `soak` | Ten minutes at capacity: N slots, N more issues always waiting, runs of 30 to 90 s that hand off, 10 events/s per agent, the default 30 s poll timer, one SSE client and a `snapshot()` every 100 ms. | Memory growth, how long a freed slot stays empty, snapshot and SSE latency, history written. |

### Definitions

- **Eligible**: the instant the harness publishes an issue in an active state to the tracker.
  **Worker started**: the first poll of that issue's worker future.
- **Integrated**: the orchestrator has processed the event (`handle_codex_event`). It bumps its
  generation counter once per event, so integrated events are an exact count read from the same
  `watch` channel the dashboard uses. In the soak the counter also moves about four times per
  finished run, which is why "integrated" there is slightly above "offered".
- **Backlog**: events emitted minus events integrated, that is, what sits in the per-run event
  channels. **Queue delay**: in a snapshot, the age of the newest "last event" among the running
  entries; with many agents emitting it is how far the orchestrator is behind. Its floor is a few
  milliseconds (timer and sampling granularity).
- **SSE frame gap**: time between two `snapshot` frames read by a real HTTP client from
  `GET /api/v1/events`. **Delivery**: time from the orchestrator reaching the generation in the
  frame's `id` (sampled every 2 ms) to the client having read the whole frame.
- Percentiles are nearest-rank over every sample of the stated window.

### Machine

| | |
|---|---|
| Hardware | MacBook Pro (Mac14,9), Apple M2 Pro, 10 cores (6 performance, 4 efficiency), 16 GB RAM, internal SSD (APFS), on mains power |
| OS | macOS 26.6.2 (25G83) |
| Toolchain | rustc 1.99.0; `bench` profile = the `release` profile (thin LTO, one codegen unit) plus symbols and function-level debug info |
| Runtime | tokio 1.54.1, multi-threaded, 10 worker threads (the binary's own configuration) |
| Store | bundled SQLite (rusqlite 0.41.1, libsqlite3-sys 0.39.1), WAL, `synchronous=NORMAL`, database in `$TMPDIR` |

It is a developer laptop, not an isolated host: Spotlight indexing and a virtual machine were
running, and the load average before the run was between 2 and 5. A second run of the same
scenarios the same morning differed by up to 15 % in throughput (for example 23,000 instead of
26,400 events/s at 500 agents at saturation, 17,100 instead of 16,100 store writes/s). Read
differences smaller than that as noise.

### Limits of the method

- The memory tracker clones its whole issue list on every lookup. In the zero-delay dispatch
  rows that is about 45 % of the poll cycle's CPU at 500 agents
  ([profile](#cpu-profiles)), a cost a real tracker does not have. A real tracker costs network
  time instead, which the 50 ms rows stand in for.
- Log lines at INFO are counted but neither formatted nor written, so production logging
  (one or two lines per dispatch and per finished run, none per event) is not in the numbers.
- The fake workers and the probes share the process and its 10 cores with the orchestrator.
  In the event profile the workers use 7 % of the process's CPU.
- The harness's `WORKFLOW.md` is 20 KB. The cost of re-reading it
  ([bottleneck 3](#bottlenecks-ranked)) grows with its size.

## Results

### 1. Dispatch latency

**Burst into free slots, refresh requested** (median of five rounds; "per dispatch" is the time
between the first and the last start divided by N − 1):

| Agents | Tracker delay | First worker | Median worker | Last worker | Per dispatch | Tracker calls | `snapshot()` max during the cycle |
|---|---|---|---|---|---|---|---|
| 10 | 0 ms | 22.8 ms | 23.5 ms | 24.4 ms | 0.17 ms | 11 | 2.4 ms |
| 100 | 0 ms | 22.9 ms | 32.8 ms | 42.2 ms | 0.19 ms | 101 | 23 ms |
| 500 | 0 ms | 22.7 ms | 89.8 ms | 153 ms | 0.26 ms | 501 | 122 ms |
| 10 | 50 ms | 127 ms | 338 ms | 598 ms | 52.3 ms | 11 | 559 ms |
| 100 | 50 ms | 128 ms | 2.70 s | 5.32 s | 52.5 ms | 101 | 5.30 s |
| 500 | 50 ms | 125 ms | 13.2 s | 26.2 s | 52.3 ms | 501 | 15.0 s (timed out in all 5 rounds) |

- The first worker never starts sooner than 20 ms after the refresh: the orchestrator waits
  `POLL_TRANSITION_RENDER_DELAY` (20 ms) between a tick and its poll cycle.
- A poll cycle makes one tracker call for the candidates and one more per issue it dispatches
  (`revalidate_issue_for_dispatch`), all awaited inside the actor loop. With a 50 ms tracker the
  cycle therefore takes N × 52 ms, and for that long the orchestrator handles nothing else:
  `snapshot()` waits for the cycle to end, and at 500 agents it gives up at its 15 s timeout
  (the HTTP API then answers `snapshot_timeout`).
- Without tracker delay a dispatch costs 0.17 to 0.26 ms of orchestrator time, of which about
  half is re-reading `WORKFLOW.md` (five times per dispatch) and most of the rest is the memory
  tracker's list copy.

**Arrivals between polls, no refresh** (30 arrivals per scale, `polling.interval_ms: 30000`, N
agents running at 10 events/s):

| Agents | Eligible → worker started: min | mean | p50 | p90 | max |
|---|---|---|---|---|---|
| 10 | 1.12 s | 15.0 s | 13.2 s | 26.7 s | 30.0 s |
| 100 | 1.13 s | 15.0 s | 13.2 s | 26.7 s | 30.0 s |
| 500 | 1.14 s | 15.0 s | 13.2 s | 26.7 s | 30.0 s |

The wait is the time to the next poll tick, uniform between zero and the interval, plus about
20 ms; the load makes no measurable difference. With the 5 s interval of this repository's
`WORKFLOW.md` the same distribution has a mean of 2.5 s.

**Freed slots** (ten-minute soak, runs of 30 to 90 s, work always waiting):

| Agents | Slots refilled | Freed slot empty: mean | p50 | p90 | max | Slots in use (mean) | Starts per refill cycle: mean (max) | Refill cycle: p50 (max) |
|---|---|---|---|---|---|---|---|---|
| 10 | 71 | 15.5 s | 16.2 s | 27.4 s | 29.3 s | 80 % | 4.2 (7) | 1.1 ms (5.7 ms) |
| 100 | 736 | 14.8 s | 14.7 s | 27.0 s | 30.0 s | 80 % | 41 (60) | 12.4 ms (23.1 ms) |
| 500 | 3,657 | 15.2 s | 15.3 s | 27.1 s | 30.1 s | 80 % | 203 (258) | 83.4 ms (105 ms) |

A slot freed by a finished run is filled only by the next poll cycle. The continuation check
one second after the exit re-examines the finished issue, not the queue. With runs of 60 s on
average and a 30 s interval, a fifth of the slots were empty at any time although work was
waiting for all of them.

### 2. Event throughput and memory

**Ramp.** "Keeps up" means the backlog grew by less than 2 % of the offered rate during the
step. Queue delay and SSE gap are p50 / max.

500 agents:

| Offered/s | Integrated/s | Backlog at step end | Queue delay (ms) | `snapshot()` p50 / p99 (ms) | SSE gap (ms) | History rows/s | RSS (MiB) | Keeps up |
|---|---|---|---|---|---|---|---|---|
| 995 | 995 | 0 | 8.2 / 260 | 2.04 / 4.59 | 205 / 260 | 620 | 41 | yes |
| 4,987 | 4,987 | 0 | 3.6 / 16 | 0.69 / 7.52 | 203 / 208 | 1,242 | 52 | yes |
| 9,967 | 9,967 | 0 | 3.8 / 22 | 0.59 / 6.08 | 203 / 207 | 1,784 | 52 | yes |
| 15,003 | 14,999 | 141 | 5.1 / 29 | 0.60 / 5.58 | 203 / 206 | 2,360 | 53 | yes |
| 19,979 | 20,002 | 10 | 4.1 / 24 | 0.66 / 5.45 | 203 / 214 | 2,917 | 51 | yes |
| 25,026 | 24,994 | 275 | 3.1 / 12 | 0.67 / 5.23 | 203 / 207 | 3,386 | 51 | yes |
| 29,990 | 26,445 | 34,578 | 31 / 258 | 0.71 / 1.55 | 203 / 204 | 3,543 | 160 | **no** |

All scales:

| Agents | Highest rate that kept up | Integrated/s once overloaded | Offered then | Backlog after the 8 s step | Queue delay then (max) | RSS idle → peak | Time to work the backlog off |
|---|---|---|---|---|---|---|---|
| 10 | 40,000/s | 39,400 | 50,000/s | 104,325 events | 2.0 s | 17 → 272 MiB | 2.6 s |
| 100 | 35,000/s | 36,100 | 40,000/s | 36,617 events | 0.54 s | 20 → 135 MiB | 1.0 s |
| 500 | 25,000/s | 26,400 | 30,000/s | 34,578 events | 0.26 s | 33 → 160 MiB | 1.3 s |

- The ceiling falls as agents are added because the work per event grows with the number of
  running entries ([bottleneck 4](#bottlenecks-ranked)): about 25 µs per event at 10 agents and
  38 µs at 500.
- Below the ceiling the queues stay empty and the queue delay stays at its floor.
- Above it the backlog grows by the difference between offered and integrated rate for as long
  as the overload lasts; nothing pushes back on the workers. Against the last step that kept
  up, resident memory grew by 2.5 KB per queued event at 10 agents, 3.0 KB at 100 and 3.3 KB at
  500; on backlogs of 1.1 to 1.2 million events (experiment runs) it was 1.4 to 1.6 KB. At 500
  agents and 30,000 offered events/s that was 109 MiB in the ten seconds of the step.
- No history event was dropped and the store queue never answered `Busy` in any ramp: at the
  ceiling the store receives 3,500 to 4,000 rows per second, about a fifth of what it can take.

**Soak**, ten minutes at 10 events/s per agent:

| Agents | Events/s | Runs started | RSS after 1 min | RSS after 10 min | RSS max | Trend | History events written | Database file | Events dropped / `Busy` |
|---|---|---|---|---|---|---|---|---|---|
| 10 | 80 | 81 | 18.0 MiB | 21.2 MiB | 22.1 MiB | +0.20 MiB/min | 12,117 | 5.8 MiB | 0 / 0 |
| 100 | 800 | 836 | 27.8 MiB | 26.1 MiB | 28.8 MiB | −0.26 MiB/min | 121,679 | 58.9 MiB | 0 / 0 |
| 500 | 3,998 | 4,157 | 51.6 MiB | 50.6 MiB | 52.0 MiB | −0.32 MiB/min | 606,899 | 295 MiB | 0 / 0 |

- Memory is flat. RSS is sampled every 5 s and swings by up to 6 MiB between samples at 500
  agents; the trend is a least-squares line through minutes 1 to 10. At 100 and 500 agents RSS
  ended below where it stood after the first minute. At 10 agents it rose by 3 MiB during the
  first four minutes and stayed between 20 and 22 MiB after that. Ten minutes are too short to
  rule a slow leak out.
- The event rate is 80 % of N × 10 because a fifth of the slots were empty
  ([section 1](#1-dispatch-latency)).
- Disk is what grows: run history took 295 MiB in ten minutes at 500 agents, about 0.5 KB per
  recorded event, 1.8 GiB per hour at this event mix.

### 3. Store throughput (SQLite)

The store alone, N writers that each await one `append_event` (a 600-byte payload) before the
next, the way a run's recorder writes. Latencies are p50 / p99 / max.

| Writers | Writes/s | Write latency (ms) | `list_runs` latency under the writes (ms) | Renew N leases (p50 / max, ms) | `Busy` |
|---|---|---|---|---|---|
| 10 | 26,853 | 0.27 / 2.41 / 23.7 | 0.35 / 3.16 / 4.81 | 0.28 / 2.08 | 0 |
| 100 | 20,513 | 3.22 / 9.48 / 20.6 | 3.97 / 9.39 / 17.4 | 4.29 / 7.51 | 0 |
| 500 | 16,100 | 28.8 / 63.8 / 152 | 30.8 / 68.3 / 112 | 31.9 / 72.5 | 0 |

- Every operation is one job on one thread with one connection, in submission order, so a read
  or a lease renewal waits behind every write queued before it: with 500 writers a `list_runs`
  takes 31 ms at the median because about 500 writes are ahead of it. Renewing 500 leases took
  72 ms at worst, against a 7.5 s renewal timeout.
- `Busy` needs more than 8,192 operations queued at once. Writers that await their writes cannot
  get there: N writers hold at most N queue slots. It was 0 in every store run above and in every
  orchestrator run on this page (the harness counts the `queue is full` warning).

Fire-and-forget flood, one task submitting without awaiting for 3 s (after the phase above):

| Runs | Offered/s | Accepted/s | Refused as `Busy` | `get_run` during the flood | Queue drained in |
|---|---|---|---|---|---|
| 10 | 782,855 | 34,370 | 95.6 % | 4 answered (p50 227 ms), 108 refused | 247 ms |
| 100 | 777,935 | 24,554 | 96.8 % | 5 answered (p50 366 ms), 80 refused | 370 ms |
| 500 | 648,331 | 12,950 | 98.0 % | 3 answered (p50 636 ms), 86 refused | 633 ms |

- When submissions outrun the thread, the queue fills within milliseconds and stays full:
  everything beyond the thread's rate is refused, reads included, and a read that does get in
  waits for a full queue (0.2 to 0.6 s).
- Every refusal logs one WARN line; the flood produced 1.9 to 2.3 million of them in 3 s.
- In the orchestrator the fire-and-forget writes are `save_retry`, `delete_retry` and
  `release_lease`, a few per run, so this needs a fault (a stalled disk, a blocked thread)
  rather than load to happen.

**PostgreSQL was not measured**: `SYMPHONY_TEST_POSTGRES_URL` was not set. The harness runs the
same store scenario against it when the variable is set.

### 4. Snapshot and SSE latency under load

During the soak (10 events/s per agent, a snapshot every 100 ms, one SSE client):

| Agents (running on average) | `snapshot()` p50 | p90 | p99 | max | Failed | SSE gap p50 / p99 / max | SSE delivery p50 / p99 / max | Frame size mean (max) |
|---|---|---|---|---|---|---|---|---|
| 10 (8) | 0.35 ms | 0.57 ms | 0.82 ms | 5.8 ms | 0 of 5,400 | 202 / 203 / 215 ms | 11.3 / 64.8 / 74.5 ms | 5 KiB (6 KiB) |
| 100 (80) | 0.29 ms | 0.83 ms | 2.14 ms | 28.4 ms | 0 of 5,389 | 202 / 207 / 277 ms | 4.3 / 61.4 / 132 ms | 41 KiB (52 KiB) |
| 500 (400) | 0.66 ms | 1.46 ms | 6.79 ms | 90.9 ms | 0 of 5,401 | 203 / 209 / 278 ms | 2.6 / 23.6 / 123 ms | 205 KiB (254 KiB) |

- A snapshot is answered by the actor between two events, so its latency is the time to finish
  the current step plus the copy of the running entries. The maxima are snapshots that waited
  for a refill cycle (83 ms at the median and 105 ms at worst at 500 agents).
- SSE frames arrive one debounce window (200 ms) apart while the state keeps changing, each a
  full state document of about 0.5 KiB per running agent: 5.7 KiB with 10 running, 51 KiB with
  100, 254 KiB with 500. With 500 running, one connected dashboard receives about 1.2 MiB per
  second (the stream is not compressed).
- Delivery is the age of the newest state change in a frame when the client has read it. At 10
  agents (80 events/s) most of it is the time since the last event; the maxima are frames that
  waited for a refill cycle.
- During the ramp the same held up to the ceiling: at 500 agents and 25,000 events/s,
  `snapshot()` p50 0.67 ms, p99 5.2 ms; SSE gap p50 203 ms, max 207 ms; delivery max 5.9 ms.
- When the actor is parked on tracker calls, both stop for that long ([section 1](#1-dispatch-latency)).

## CPU profiles

Recorded with [samply](https://github.com/mstange/samply) 0.13.1 at 1 kHz on the same build,
500 agents each, and rendered with `inferno-flamegraph`
([how](#reproducing)). Widths are on-CPU time: samples of parked threads are dropped. A stack
that ends in `open`, `pwrite` or `fsync` is time inside the kernel, on CPU or waiting for the
disk. Shares are of the whole process, harness included.

| Flamegraph | Load | Sampled |
|---|---|---|
| [`flamegraph-events-500.svg`](performance/flamegraph-events-500.svg) | `events`, 15,000 events/s held for 30 s | 28.1 s on CPU |
| [`flamegraph-dispatch-500.svg`](performance/flamegraph-dispatch-500.svg) | `dispatch`, 20 bursts of 500, no tracker delay | 4.5 s on CPU |
| [`flamegraph-store-500.svg`](performance/flamegraph-store-500.svg) | `store`, 500 writers for 20 s, then the flood | 27.8 s on CPU |

Each has a `.top.txt` next to it with the 40 largest functions by self time and by inclusive time.

**Events, 500 agents, 15,000 events/s.** The orchestrator task takes 68.7 % of the process's CPU
(about 0.6 of one core, with the ceiling at one core).

| Share | Frame (inclusive) |
|---|---|
| 60.3 % | `Orchestrator::enforce_spend`, called after every event |
| 39.6 % | ↳ `WorkflowStore::reload` → `current_stamp`: `open` 19.4 %, SipHash of the file 15.5 %, `stat` 3.9 %, `read` 2.9 % |
| 17.4 % | ↳ `Spend::issue_stop` for every running issue (string hash and compare per entry) |
| 5.0 % | `Orchestrator::handle_codex_event` (find the entry by run id, integrate, record) |
| 18.8 % | `symphony-store` thread: `ops::append_event` 14.9 %, `ops::update_usage` 2.9 %; `fsync` 6.4 %, `pwrite` 6.1 % |
| 7.5 % | the harness's fake workers |
| 1.0 % | `Orchestrator::snapshot`; 0.6 % HTTP server and SSE; 0.2 % `watch::Sender::send_modify` (`notify`) |

Top self-time frames: `__open` 19.4 %, `core::hash::sip::Hasher::write` 15.5 %, `fsync` 6.4 %,
`pwrite` 6.1 %, `RandomState::hash_one::<&str>` 4.2 %, `hashbrown::RawTable<(String, Usage)>::len` 4.0 %,
`stat` 3.9 %, `_platform_memcmp` 3.7 %.

**Dispatch, 500 agents.** The poll cycle (`run_poll_cycle` → `maybe_dispatch`) takes 60.0 %.

| Share | Frame (inclusive) |
|---|---|
| 29.0 % | `WorkflowStore::reload` (every `settings()` call of the dispatch path) |
| 26.9 % | `MemoryTracker` / `MemoryIssues::get`: the whole issue list cloned per lookup (harness artifact) |
| 11.9 % | `spawn_issue_on_worker_host` (most of it the two `settings()` calls inside) |
| 2.5 % | `State::state_slots_available` (normalises the state of every running entry per candidate) |
| 28.7 % | `symphony-store` thread (run rows, leases, retry rows) |

Top self-time frames: `__open` 14.4 %, `pwrite` 10.9 %, `core::intrinsics::rotate_left::<u64>`
(SipHash) 8.5 %, `_platform_memmove` 4.3 %, `fsync` 3.3 %, `stat` 3.3 %.

**Store, 500 writers.** The `symphony-store` thread takes 84.5 %, all of one core.

| Share | Frame |
|---|---|
| 81.2 % | `ops::append_event` (inclusive) |
| 41.8 % | SQLite WAL checkpoints (inclusive; every 1,000 WAL pages) |
| 31.6 % | `fsync` (self) |
| 27.0 % | `pwrite` (self) |
| 7.6 % | `pread` (self) |
| 7.9 % | JSON encoding of payloads (inclusive) |

## Bottlenecks, ranked

Ranked by how much each limits a 500-agent deployment that wants to call itself fast.

1. **Work starts only on the poll timer.** Evidence: eligible → started is 15.0 s on average and
   30.0 s at worst with the default interval, at every scale; a freed slot stays empty for
   15.2 s on average and 30.1 s at worst at 500 agents, which left 80 % of the slots in use
   although work was always waiting. Polling on every worker exit
   ([experiment X5](#throwaway-experiments)) cut the empty time to 30 ms and raised use to
   100 %, at the price of more tracker calls.
2. **Tracker calls are awaited inside the actor loop, one per dispatched issue.** Evidence: with
   50 ms per call a dispatch takes 52 ms and 500 dispatches park the orchestrator for 26 s;
   `snapshot()` timed out at 15 s in all five rounds; events and lease renewals wait as long.
   In steady state the same happens at every poll. A three-minute soak with a 50 ms tracker
   ([results](performance/results/slow-tracker)) parked the actor for 2.1 s per refill cycle at
   100 agents (35 dispatches) and for 11.2 s at 500 (210 dispatches; 15.9 s at worst). At 500
   agents events waited up to 16.7 s to be integrated, the SSE stream paused for 15.2 s, one of
   904 snapshots timed out, resident memory rose from 51 MiB to as much as 150 MiB while events
   queued, and slots in use fell from 80 % to 69 %.
3. **`WORKFLOW.md` is read and hashed on every `settings()` call.** `WorkflowStore::settings()`
   calls `reload()`, which stats, reads and SipHashes the file (`current_stamp`,
   `crates/symphony-core/src/workflow_store.rs`). `enforce_spend()` asks for the settings after
   every agent event; a dispatch asks five times. Evidence: 39.6 % of process CPU in the event
   profile and 29.0 % in the dispatch profile; it is why the event ceiling is 25,000 to 40,000
   events/s. [Experiment X1](#throwaway-experiments) (use the already loaded settings in
   `enforce_spend`) raised the ceiling to 50,000 at 500 agents and to 200,000 or more at 10 and
   100.
4. **Work per event grows with the number of running agents.** `enforce_spend()` checks the
   budget of every running issue after each event (17.4 % of CPU at 500 agents), and
   `handle_codex_event` finds its entry by scanning the running entries. Evidence: the ceiling
   falls from 40,000 events/s at 10 agents to 25,000 at 500; with X1 in place the gap is
   wider (400,000 against 50,000). [Experiment X2](#throwaway-experiments) (an index by run id
   and a check of only the issue whose event arrived) reached 400,000 events/s at 500 agents.
5. **Event channels are unbounded.** Evidence: past the ceiling the backlog grew for as long
   as the overload lasted (34,578 events in 8 s at 500 agents with 30,000 offered), resident
   memory grew by 1.5 to 3.3 KB per queued event, the dashboard fell behind by the queue delay,
   and nothing slowed the producers. The process recovers when the load drops (the backlog above
   was worked off in 1.3 s) but does not protect itself while it lasts.
6. **The store is one thread with one connection and one transaction per write.** Evidence:
   16,100 writes/s at 500 runs, 26,900 at 10; reads and lease renewals queue behind writes
   (31 ms median at 500 writers). It is not the limit today: at the orchestrator's ceiling the
   store gets 3,500 to 4,000 rows/s. It becomes the limit once 3 and 4 are fixed: in the X2 runs at
   200,000 events/s and more, each ramp lost 150,000 to 716,000 history events to full recorder
   queues, and those queues held several hundred MiB. Grouping queued writes into one
   transaction ([X3](#throwaway-experiments)) helps few runs (26,900 → 83,000 writes/s at 10) and
   not many (16,100 → 16,400 at 500), because each run dirties its own pages.
7. **Snapshots and SSE frames grow with the number of agents**, 254 KiB per frame at 500. Not a
   limit at this scale (`snapshot()` p99 6.8 ms), but each dashboard costs 1.2 MiB/s of
   loopback or network at 500 agents.

## The review's suspects

| Suspect | Verdict | Evidence |
|---|---|---|
| One database thread and connection behind a FIFO queue of 8,192 | **Confirmed as a ceiling and as head-of-line blocking; `Busy` ruled out under load.** | 16,100 to 26,900 writes/s; reads wait behind writes (31 ms median, 68 ms p99 at 500 writers); `Busy` never occurred in any orchestrator run and needs a fire-and-forget flood (then 96 to 98 % refused). Not the first limit: bottlenecks 3 and 4 cap the orchestrator below a quarter of the store's rate. |
| Linear scan of running entries on every event | **Confirmed, and there is a second, larger scan.** | `handle_codex_event` is 5.0 % of CPU at 500 agents; the budget check over all running issues in `enforce_spend` is 17.4 %. Removing both (X2) took 500 agents from 50,000 to 400,000 events/s with X1 applied. |
| Dashboard notify on every event | **Ruled out as a cost.** | `watch::Sender::send_modify` is 0.2 % of CPU at 15,000 events/s. The SSE hub debounces to one snapshot per 200 ms whatever the event rate (gap p50 201 to 206 ms at every step). |
| Unbounded channels for timers, worker messages and per-run event streams | **Confirmed for the event streams; the other two carry too little to matter.** | Backlog and memory growth above. Timers carry two events per poll and one per retry, worker messages one per run: under 20 per second in the 500-agent soak. Their depths were not measured directly. |
| Tracker calls awaited inline in the actor loop | **Confirmed.** | Dispatch rows with 50 ms tracker delay: 52 ms per dispatch, 26 s for 500, `snapshot()` timeouts. |

Found without being suspected: the `WORKFLOW.md` re-read (bottleneck 3), the poll-bound refill
(bottleneck 1) and the fixed 20 ms before every poll cycle.

## Throwaway experiments

Each was a patch on `f877d3a`, measured with the same harness on the same machine and reverted.
None is on this branch, and none was reviewed for correctness or run against the test suite.

| | Change | Measured | Before | After |
|---|---|---|---|---|
| X1 | `enforce_spend` uses the settings already loaded (`WorkflowStore::last_good`) instead of `settings()` | Highest event rate that kept up, 10 / 100 / 500 agents | 40,000 / 35,000 / 25,000 per s | 400,000 (the top of the ramp) / 200,000 / 50,000 per s (64,300 integrated when 70,000 were offered) |
| X2 | X1, plus a `run id → issue id` map for `handle_codex_event`, plus a spend check of only the issue whose event arrived | Highest event rate that kept up, 10 / 100 / 500 agents | 40,000 / 35,000 / 25,000 per s | 200,000 (280,000 integrated when 300,000 were offered) / 400,000 / 400,000 per s |
| X3 | The store thread runs every already queued operation (up to 256) in one SQLite transaction | Store writes/s, 10 / 100 / 500 writers | 26,900 / 20,500 / 16,100 | 83,000 / 28,000 / 16,400 |
| X4 | `PRAGMA wal_autocheckpoint = 16384` (default 1,000 pages) | Store writes/s, 10 / 100 / 500 writers | 26,900 / 20,500 / 16,100 | 31,100 / 24,600 / 23,500 |
| X3 + X4 | Both | Store writes/s, 10 / 100 / 500 writers | 26,900 / 20,500 / 16,100 | 73,200 / 33,900 / 23,000 |
| X5 | `request_refresh()` when a worker exits | Freed slot empty, mean / max; slots in use; tracker calls per minute (3-minute soak, 100 / 500 agents) | 14.8 s / 30.0 s and 15.2 s / 30.1 s; 80 %; 165 and 803 (ten-minute soak) | 27 ms / 32 ms and 30 ms / 118 ms; 100 %; 370 and 1,724 |

What each would need before it could be a change:

- X1: settings would follow an edited `WORKFLOW.md` at the next poll tick, snapshot or
  workflow-poller pass (1 s) instead of at the next event.
- X2: the map must be pruned when a run ends (the patch leaks one entry per run), and the
  narrower spend check must keep the daily-budget and kill-switch behaviour (the patch falls
  back to the full check when a global stop applies).
- X3: replies are sent before the group commits, so a failed commit would lose writes already
  acknowledged.
- X4: a larger WAL (64 MiB before a checkpoint) and longer recovery after a crash.
- X5: every exit runs a full poll cycle (reconcile, candidate fetch, one call per dispatch);
  with a hosted tracker that needs rate-limit budgeting or a narrower refill path.

The ramps of X1 and X2 used coarser steps than the documented run (50,000, 70,000, 100,000,
150,000, 200,000, 300,000, 400,000 and 600,000 events/s). The "before" store numbers are from the
documented run; the store experiments ran 40 minutes earlier, when an unpatched run measured
25,300 / 17,800 / 17,100 writes/s. With ten workers at 20,000 events/s or more each, the 10-agent
figures are limited by the harness's workers as much as by the orchestrator.

## Not measured

- **PostgreSQL**, and with it lease contention between replicas, the lease watchdog and
  recovery of another replica's work. `SYMPHONY_TEST_POSTGRES_URL` was not set.
- **Real agents**: Codex or Claude Code processes, their stdout parsing, workspaces, hooks, git
  clones and process-group cleanup. The fake worker skips all of it, so per-run CPU, memory
  and file-descriptor costs are unknown.
- **Real trackers**: only a fixed 0 or 50 ms delay; no rate limits, pagination, failures or
  retries.
- **Linux and container limits**: one macOS laptop. SQLite `fsync` behaves differently on
  Linux, and nothing ran under a cgroup memory limit.
- **Several replicas, SSH workers, the task API** (`POST /api/v1/tasks` and its dispatch path),
  and **more than one SSE client**.
- **Budgets**: no `budget` keys were set, so the per-issue checks ran but never stopped a run.
- **Soaks longer than ten minutes** and stores larger than what ten minutes write
  (295 MiB at 500 agents); retention and pruning under load.
- **Depths of the timer and worker-message channels** (argued from their rates, not observed).
- **Production logging cost** and **TLS or a reverse proxy** in front of the API.
- **Variance**: one run per configuration (five rounds for bursts), no confidence intervals.

## Reproducing

```bash
cargo bench -p symphony --bench load                          # everything, about 50 minutes
cargo bench -p symphony --bench load -- events --agents 500   # one scenario at one scale
cargo bench -p symphony --bench load -- --help                # every knob
```

Results are printed and written to `target/perf/<scenario>-<agents>.json`. With
`SYMPHONY_TEST_POSTGRES_URL` set, `store` also runs against PostgreSQL. The soak with a slow
tracker was `soak --agents 100,500 --soak-secs 180 --soak-tracker-latency-ms 50`.

CPU profiles need two developer tools, `cargo install --locked samply inferno`, and no `sudo`:

```bash
BIN=$(cargo bench -p symphony --bench load --no-run 2>&1 | sed -n 's/.*Executable.*(\(.*\))/\1/p')
samply record --save-only --unstable-presymbolicate -o target/perf/profile-events-500.json -- \
  "$BIN" events --agents 500 --rates 15000 --step-secs 30
"$BIN" fold --profile target/perf/profile-events-500.json
inferno-flamegraph --countname us target/perf/profile-events-500.folded > flamegraph-events-500.svg
```

The other two profiles used `dispatch --agents 500 --tracker-latency-ms 0 --rounds 20
--arrival-intervals 0` and `store --agents 500 --store-secs 20`. `fold` is part of the harness:
it resolves samply's symbol sidecar, keeps on-CPU samples and writes the folded stacks plus the
`.top.txt` list.
