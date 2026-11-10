//! Load harness for the orchestrator and the store. Method and results: `docs/performance.md`.
//!
//! ```text
//! cargo bench -p symphony --bench load                      # every scenario at 10, 100 and 500 agents
//! cargo bench -p symphony --bench load -- events --agents 500
//! ```
//!
//! It runs the real runtime, store and HTTP server in this process with `tracker.kind: memory`
//! and a fake worker ([`world`]); no agent process is started and no model is called. Every
//! scenario and scale runs in a process of its own, so memory left behind by one run is never
//! counted in the next.

mod probes;
mod profile;
mod rig;
mod scenarios;
mod stats;
mod store_bench;
mod world;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use serde_json::{Value, json};

use crate::rig::StoreKind;
use crate::scenarios::SoakConfig;
use crate::store_bench::Backend;

/// Result type of the harness (errors are only reported).
pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Environment variable naming a PostgreSQL database for the store benchmark.
const POSTGRES_URL: &str = "SYMPHONY_TEST_POSTGRES_URL";
/// Set by the harness for the processes it starts: `<scenario>:<agents>`, the one run to do.
const ONLY: &str = "SYMPHONY_LOAD_ONLY";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Scenario {
    /// Everything below, in this order.
    All,
    /// Dispatch latency: a burst into free slots with a refresh, and arrivals between polls.
    Dispatch,
    /// Event throughput: a stepped ramp of the total event rate.
    Events,
    /// The store alone: writes per second, read latency under writes, `Busy`.
    Store,
    /// Steady state at capacity for `--soak-secs`: memory, freed slots, snapshot and SSE latency.
    Soak,
    /// Not a measurement: fold the samply profile at `--profile` for `inferno-flamegraph`.
    Fold,
}

#[derive(Debug, Parser)]
#[command(name = "load", about = "Symphony load harness (docs/performance.md)")]
struct Cli {
    /// What to run.
    #[arg(value_enum, default_value_t = Scenario::All)]
    scenario: Scenario,
    /// Concurrently running agents (one run of each scenario per value).
    #[arg(long, value_delimiter = ',', default_value = "10,100,500")]
    agents: Vec<u32>,
    /// Directory of the result files.
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/perf"))]
    out: PathBuf,
    /// Run history store of the orchestrator scenarios.
    #[arg(long, value_enum, default_value_t = StoreKind::Sqlite)]
    store: StoreKind,
    /// dispatch: delay added to every tracker call, one burst series per value.
    #[arg(long, value_delimiter = ',', default_value = "0,50")]
    tracker_latency_ms: Vec<u64>,
    /// dispatch: bursts per configuration.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
    /// dispatch (arrivals) and soak: `polling.interval_ms`.
    #[arg(long, default_value_t = 30_000)]
    poll_interval_ms: u64,
    /// dispatch (arrivals): poll intervals to observe (0 skips the arrivals part).
    #[arg(long, default_value_t = 3)]
    arrival_intervals: u32,
    /// dispatch (arrivals) and soak: events per second of each agent.
    #[arg(long, default_value_t = 10.0)]
    rate: f64,
    /// events: total events per second of each ramp step.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1000,2000,5000,10000,15000,20000,25000,30000,35000,40000,50000,70000,100000,150000,200000,400000"
    )]
    rates: Vec<u64>,
    /// events: seconds measured per step (after two seconds of settling).
    #[arg(long, default_value_t = 8)]
    step_secs: u64,
    /// store: seconds of the closed-loop write phase.
    #[arg(long, default_value_t = 10)]
    store_secs: u64,
    /// soak: duration in seconds.
    #[arg(long, default_value_t = 600)]
    soak_secs: u64,
    /// soak: mean length of a fake run in seconds.
    #[arg(long, default_value_t = 60)]
    run_secs: u64,
    /// soak: delay added to every tracker call.
    #[arg(long, default_value_t = 0)]
    soak_tracker_latency_ms: u64,
    /// fold: a profile written by `samply record --save-only --unstable-presymbolicate -o
    /// <name>.json` (its `<name>.syms.json` must be next to it).
    #[arg(long)]
    profile: Option<PathBuf>,
    /// Passed by `cargo bench`; ignored.
    #[arg(long, hide = true)]
    bench: bool,
}

fn write_results(cli: &Cli, name: &str, results: Vec<Value>) -> Res<()> {
    std::fs::create_dir_all(&cli.out)?;
    let scale = cli.agents.first().copied().unwrap_or(0);
    let path = cli.out.join(format!("{name}-{scale}.json"));
    let document = json!({
        "scenario": name,
        "recorded_at": chrono::Utc::now().to_rfc3339(),
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "logical_cpus": std::thread::available_parallelism().map_or(0, usize::from),
        },
        "results": results,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&document)? + "\n")?;
    println!("  -> {}", path.display());
    Ok(())
}

async fn dispatch(cli: &Cli) -> Res<()> {
    println!("dispatch latency");
    let mut bursts = Vec::new();
    for &agents in &cli.agents {
        for &latency in &cli.tracker_latency_ms {
            bursts.push(scenarios::dispatch_burst(agents, latency, cli.rounds, cli.store).await?);
        }
    }
    write_results(cli, "dispatch-burst", bursts)?;
    if cli.arrival_intervals == 0 {
        return Ok(());
    }
    let mut arrivals = Vec::new();
    for &agents in &cli.agents {
        arrivals.push(
            scenarios::arrivals(
                agents,
                cli.poll_interval_ms,
                cli.rate,
                cli.arrival_intervals,
                cli.store,
            )
            .await?,
        );
    }
    write_results(cli, "dispatch-arrivals", arrivals)
}

async fn events(cli: &Cli) -> Res<()> {
    println!("event throughput");
    let mut results = Vec::new();
    for &agents in &cli.agents {
        let mut result =
            scenarios::event_ramp(agents, &cli.rates, cli.step_secs, cli.store).await?;
        result["log"] = probes::log_counts().json();
        probes::log_counts().reset();
        results.push(result);
    }
    write_results(cli, "events", results)
}

async fn store(cli: &Cli) -> Res<()> {
    println!("store throughput");
    let mut backends = vec![Backend::Sqlite];
    match std::env::var(POSTGRES_URL) {
        Ok(url) if !url.trim().is_empty() => backends.push(Backend::Postgres(url)),
        _ => println!("  {POSTGRES_URL} is not set: PostgreSQL is not measured"),
    }
    let mut results = Vec::new();
    for backend in &backends {
        for &writers in &cli.agents {
            let mut result = store_bench::store_bench(writers, cli.store_secs, backend).await?;
            result["log"] = probes::log_counts().json();
            probes::log_counts().reset();
            results.push(result);
        }
    }
    write_results(cli, "store", results)
}

async fn soak(cli: &Cli) -> Res<()> {
    println!("soak");
    let mut results = Vec::new();
    for &agents in &cli.agents {
        let mut result = scenarios::soak(SoakConfig {
            agents,
            secs: cli.soak_secs,
            rate: cli.rate,
            run_secs: cli.run_secs,
            poll_interval_ms: cli.poll_interval_ms,
            tracker_latency: std::time::Duration::from_millis(cli.soak_tracker_latency_ms),
            store: cli.store,
        })
        .await?;
        result["log"] = probes::log_counts().json();
        probes::log_counts().reset();
        results.push(result);
    }
    write_results(cli, "soak", results)
}

fn fold(cli: &Cli) -> Res<()> {
    let profile = cli
        .profile
        .as_deref()
        .ok_or("fold needs --profile <file>")?;
    let (folded, top) = profile::fold(profile)?;
    println!(
        "folded stacks: {}\ntop functions: {}",
        folded.display(),
        top.display()
    );
    Ok(())
}

async fn run(cli: Cli) -> Res<()> {
    probes::log_counts();
    match cli.scenario {
        Scenario::Dispatch => dispatch(&cli).await,
        Scenario::Events => events(&cli).await,
        Scenario::Store => store(&cli).await,
        Scenario::Soak => soak(&cli).await,
        Scenario::All | Scenario::Fold => Ok(()),
    }
}

/// Runs each scenario at each scale in a fresh process (this executable again, told through
/// [`ONLY`] which single run to do), so no run inherits another's memory.
fn run_isolated(cli: &Cli) -> Res<()> {
    let scenarios: &[Scenario] = match cli.scenario {
        Scenario::All => &[
            Scenario::Dispatch,
            Scenario::Events,
            Scenario::Store,
            Scenario::Soak,
        ],
        ref one => std::slice::from_ref(one),
    };
    let executable = std::env::current_exe()?;
    for scenario in scenarios {
        let name = scenario
            .to_possible_value()
            .map(|value| value.get_name().to_owned())
            .unwrap_or_default();
        for agents in &cli.agents {
            let status = std::process::Command::new(&executable)
                .args(std::env::args_os().skip(1))
                .env(ONLY, format!("{name}:{agents}"))
                .status()?;
            if !status.success() {
                return Err(format!("{name} at {agents} agents failed ({status})").into());
            }
        }
    }
    Ok(())
}

/// The single run this process was started for, if [`ONLY`] is set.
fn only() -> Option<(Scenario, u32)> {
    let value = std::env::var(ONLY).ok()?;
    let (scenario, agents) = value.split_once(':')?;
    Some((
        Scenario::from_str(scenario, true).ok()?,
        agents.parse().ok()?,
    ))
}

fn main() -> ExitCode {
    let mut cli = Cli::parse();
    if let Some((scenario, agents)) = only() {
        cli.scenario = scenario;
        cli.agents = vec![agents];
    }
    let outcome = match cli.scenario {
        Scenario::Fold => fold(&cli),
        // One scenario at one scale runs here; anything more is split into such runs.
        Scenario::All => run_isolated(&cli),
        _ if cli.agents.len() != 1 => run_isolated(&cli),
        // The runtime the binary builds (`app::main`): multi-threaded, one worker per core.
        _ => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("symphony")
            .build()
            .map_err(Into::into)
            .and_then(|runtime| runtime.block_on(run(cli))),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("load harness failed: {err}");
            ExitCode::FAILURE
        }
    }
}
