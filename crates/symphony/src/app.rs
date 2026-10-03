//! Process bootstrap and lifecycle (Elixir `SymphonyElixir.Application` + `CLI.main/1`,
//! blueprint §1.5 / A.1).
//!
//! ```text
//! cli::evaluate            usage / banner / not found -> exit 1 (before anything starts)
//! WorkflowStore::start     invalid workflow -> "Failed to start Symphony with workflow ..." exit 1
//! logging::init            rotating file (+ stdout unless the terminal dashboard owns it)
//! Store                    open, close runs left `running`, prune (+ every 24 h)
//! Runtime::start           orchestrator supervisor
//! symphony_server::serve   when a port is configured; bind failure aborts startup
//! dashboard::run           when enabled and stdout is a terminal
//! wait                     SIGINT/SIGTERM -> graceful stop, exit 0; runtime fatal -> exit 1
//! ```

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use symphony_core::WorkflowStore;
use symphony_runtime::{Runtime, RuntimeError, RuntimeOptions};
use symphony_server::{BoundServer, ServerConfig};
use symphony_store::{
    DEFAULT_KEEP_MIN_RUNS, DEFAULT_PRUNE_INTERVAL, ENV_DB_PATH, Store, StoreConfig, StoreLocation,
};
use symphony_trackers::TrackerDeps;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::before_remove::{self, SystemRunner};
use crate::cli::{self, CliContext, CliExit, Command, DbChoice, Invocation};
use crate::control::RuntimeControlPlane;
use crate::dashboard::{self, RuntimeSource};
use crate::logging::{self, ConsoleFormat, ENV_LOG_FORMAT, Logging};
use crate::memory_seed;
use crate::version::version;

/// Upper bound for flushing the run-history store on shutdown.
const STORE_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for the tokio runtime to wind down blocking work after `main` finished.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The `symphony` binary: evaluates the command line and runs the requested command.
pub fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let env = |name: &str| std::env::var(name).ok();
    let ctx = CliContext {
        cwd,
        env: &env,
        file_regular: &cli::file_regular,
    };
    match cli::evaluate(&args, &ctx) {
        Err(exit) => print_exit(&exit),
        Ok(Command::BeforeRemove { branch, repo }) => {
            let runner = SystemRunner::from_env();
            before_remove::run(
                branch,
                &repo,
                &runner,
                &mut std::io::stdout().lock(),
                &mut std::io::stderr().lock(),
            );
            ExitCode::SUCCESS
        }
        Ok(Command::Run(invocation)) => run(invocation),
    }
}

fn print_exit(exit: &CliExit) -> ExitCode {
    let message = exit.message();
    if exit.is_stdout() {
        let _ = writeln!(std::io::stdout().lock(), "{message}");
    } else {
        let _ = writeln!(std::io::stderr().lock(), "{message}");
    }
    ExitCode::from(exit.exit_code())
}

fn fail(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "{message}");
    ExitCode::FAILURE
}

/// Boots Symphony for a validated invocation and blocks until it stops.
pub fn run(invocation: Invocation) -> ExitCode {
    let workflow_path = invocation.workflow.display().to_string();
    let workflow = match WorkflowStore::start(Some(invocation.workflow.clone())) {
        Ok(workflow) => workflow,
        Err(err) => {
            return fail(&format!(
                "Failed to start Symphony with workflow {workflow_path}: {}",
                err.user_message()
            ));
        }
    };
    let console_format = match ConsoleFormat::parse(std::env::var(ENV_LOG_FORMAT).ok().as_deref()) {
        Ok(format) => format,
        Err(message) => return fail(&message),
    };
    let stdout_is_tty = std::io::stdout().is_terminal();
    let dashboard_enabled = workflow.settings().observability.dashboard_enabled && stdout_is_tty;
    let console = (!dashboard_enabled).then_some(console_format);
    let logging = logging::init(&invocation.logs_root, console);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("symphony")
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(&format!("Failed to start the async runtime: {err}")),
    };
    let code = runtime.block_on(serve_until_stopped(
        invocation,
        workflow,
        dashboard_enabled,
        logging.clone(),
    ));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    logging.flush();
    ExitCode::from(code)
}

/// Opens the run-history store selected by the flags and `SYMPHONY_DB_*`.
fn store_config(choice: &DbChoice) -> Result<StoreConfig, String> {
    let config = match choice {
        DbChoice::Disabled => Ok(StoreConfig {
            location: StoreLocation::Disabled,
            retention: None,
        }),
        DbChoice::Path(path) => StoreConfig::from_lookup(|name| {
            if name == ENV_DB_PATH {
                Some(path.clone())
            } else {
                std::env::var(name).ok()
            }
        }),
        DbChoice::FromEnv => StoreConfig::from_env(),
    };
    config.map_err(|err| err.to_string())
}

async fn open_store(choice: &DbChoice, cancel: &CancellationToken) -> Result<Store, String> {
    let config = store_config(choice)?;
    let store = config.open().map_err(|err| {
        format!("Failed to open the run history database: {err} (use --no-db or SYMPHONY_DB_PATH=off to run without history)")
    })?;
    match &config.location {
        StoreLocation::Disabled => tracing::info!("Run history disabled"),
        StoreLocation::Memory => tracing::info!("Run history kept in memory (lost on exit)"),
        StoreLocation::File(path) => {
            tracing::info!("Run history database path={}", path.display());
        }
    }
    if !store.is_enabled() {
        return Ok(store);
    }
    match store.mark_interrupted_runs().await {
        Ok(0) => {}
        Ok(count) => {
            tracing::info!("Closed runs interrupted by the previous process count={count}")
        }
        Err(err) => tracing::warn!("Failed to close interrupted runs: {err}"),
    }
    if let Some(retention) = config.retention {
        prune(&store, retention).await;
        let store = store.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(DEFAULT_PRUNE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => prune(&store, retention).await,
                }
            }
        });
    }
    Ok(store)
}

async fn prune(store: &Store, retention: Duration) {
    match store.prune(retention, DEFAULT_KEEP_MIN_RUNS).await {
        Ok(stats) if stats.runs_deleted > 0 => tracing::info!(
            "Pruned run history runs_deleted={} events_deleted={}",
            stats.runs_deleted,
            stats.events_deleted
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!("Failed to prune run history: {err}"),
    }
}

/// SIGINT / SIGTERM.
struct Signals {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn new() -> Self {
        Self {
            #[cfg(unix)]
            term: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok(),
        }
    }

    async fn next(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            let term = async {
                match self.term.as_mut() {
                    Some(term) => {
                        term.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                () = term => "SIGTERM",
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        }
    }
}

/// Running services that must be stopped in order.
struct Services {
    handle: symphony_runtime::RuntimeHandle,
    runtime: JoinHandle<Result<(), RuntimeError>>,
    server: Option<BoundServer>,
    server_cancel: CancellationToken,
    dashboard: Option<JoinHandle<()>>,
    dashboard_cancel: CancellationToken,
    background_cancel: CancellationToken,
    store: Store,
}

async fn serve_until_stopped(
    invocation: Invocation,
    workflow: Arc<WorkflowStore>,
    dashboard_enabled: bool,
    logging: Logging,
) -> u8 {
    let workflow_path = invocation.workflow.display().to_string();
    tracing::info!(
        "Starting Symphony version={} workflow={workflow_path} log_file={}",
        version(),
        logging.file.as_ref().map_or_else(
            || "none".to_owned(),
            |file| file.path().display().to_string()
        )
    );
    let mut signals = Signals::new();
    let background_cancel = CancellationToken::new();
    let _poller = workflow.spawn_poller(background_cancel.clone());

    let store = match open_store(&invocation.db, &background_cancel).await {
        Ok(store) => store,
        Err(message) => {
            tracing::error!("{message}");
            background_cancel.cancel();
            let _ = writeln!(std::io::stderr().lock(), "{message}");
            return 1;
        }
    };

    let deps = match TrackerDeps::new() {
        Ok(deps) => deps,
        Err(err) => {
            let message = format!(
                "Failed to start Symphony with workflow {workflow_path}: tracker HTTP client: {err}"
            );
            tracing::error!("{message}");
            let _ = writeln!(std::io::stderr().lock(), "{message}");
            background_cancel.cancel();
            return 1;
        }
    };
    let _seeder = memory_seed::spawn(
        Arc::clone(&workflow),
        deps.memory.clone(),
        background_cancel.clone(),
    );

    let runtime =
        Runtime::start(RuntimeOptions::new(Arc::clone(&workflow), deps).with_store(store.clone()));
    let handle = runtime.handle();
    let mut services = Services {
        handle: handle.clone(),
        runtime: tokio::spawn(runtime.wait()),
        server: None,
        server_cancel: CancellationToken::new(),
        dashboard: None,
        dashboard_cancel: CancellationToken::new(),
        background_cancel,
        store,
    };

    let settings = workflow.settings();
    let host = invocation
        .host
        .clone()
        .unwrap_or_else(|| settings.server.host.clone());
    let configured_port = invocation.port.or(settings.server.port);
    let mut bound_port = None;
    if let Some(port) = configured_port {
        let config = ServerConfig {
            version: version().to_owned(),
            ..ServerConfig::new(host.clone(), port)
        };
        let control = Arc::new(RuntimeControlPlane::new(
            handle.clone(),
            Arc::clone(&workflow),
        ));
        match symphony_server::serve(
            config,
            control,
            services.store.clone(),
            services.server_cancel.clone(),
        )
        .await
        {
            Ok(server) => {
                bound_port = Some(server.port());
                let url = dashboard::format::dashboard_url(&host, Some(port), Some(server.port()))
                    .unwrap_or_else(|| format!("http://{}/", server.local_addr()));
                tracing::info!("Symphony listening on {url} addr={}", server.local_addr());
                let mut stdout = std::io::stdout().lock();
                let _ = writeln!(stdout, "Symphony listening on {url}");
                let _ = stdout.flush();
                services.server = Some(server);
            }
            Err(err) => {
                let message =
                    format!("Failed to start Symphony with workflow {workflow_path}: {err}");
                tracing::error!("{message}");
                let _ = writeln!(std::io::stderr().lock(), "{message}");
                stop(&mut services, &mut signals, false).await;
                return 1;
            }
        }
    }

    if dashboard_enabled {
        let source = Arc::new(RuntimeSource {
            handle: handle.clone(),
            workflow: Arc::clone(&workflow),
            host,
            configured_port,
            bound_port,
        });
        services.dashboard = Some(tokio::spawn(dashboard::run(
            source,
            Box::new(dashboard::render_to_terminal),
            services.dashboard_cancel.clone(),
        )));
    }

    tokio::select! {
        signal = signals.next() => {
            tracing::info!("Received {signal}; shutting down");
            if stop(&mut services, &mut signals, dashboard_enabled).await {
                0
            } else {
                1
            }
        }
        outcome = &mut services.runtime => {
            let code = match outcome {
                Ok(Ok(())) => {
                    tracing::info!("Agent runtime stopped");
                    0
                }
                Ok(Err(err)) => {
                    tracing::error!("Agent runtime failed: {err}");
                    let _ = writeln!(std::io::stderr().lock(), "Symphony runtime failed: {err}");
                    1
                }
                Err(err) => {
                    tracing::error!("Agent runtime task failed: {err}");
                    1
                }
            };
            services.runtime = tokio::spawn(async { Ok(()) });
            stop(&mut services, &mut signals, dashboard_enabled).await;
            code
        }
    }
}

/// Graceful stop: dashboard, runtime (workers cancelled, then aborted), HTTP server drain,
/// background tasks, store flush, offline frame. Returns `false` when a second signal forced
/// the exit.
async fn stop(services: &mut Services, signals: &mut Signals, offline_frame: bool) -> bool {
    services.dashboard_cancel.cancel();
    if let Some(dashboard) = services.dashboard.take() {
        let _ = dashboard.await;
    }
    let graceful = async {
        services.handle.shutdown();
        match (&mut services.runtime).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::error!("Agent runtime stopped with an error: {err}"),
            Err(err) => tracing::error!("Agent runtime task failed: {err}"),
        }
        services.server_cancel.cancel();
        if let Some(server) = services.server.take()
            && let Err(err) = server.wait().await
        {
            tracing::warn!("HTTP server stopped with an error: {err}");
        }
        services.background_cancel.cancel();
        match tokio::time::timeout(STORE_FLUSH_TIMEOUT, services.store.flush()).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!("Failed to flush run history: {err}"),
            Err(_) => tracing::warn!("Timed out flushing run history"),
        }
    };
    let forced = tokio::select! {
        () = graceful => false,
        signal = signals.next() => {
            tracing::warn!("Received {signal} again; forcing exit");
            let _ = writeln!(std::io::stderr().lock(), "Received {signal} again; forcing exit");
            true
        }
    };
    if forced {
        return false;
    }
    if offline_frame {
        dashboard::render_to_terminal(&dashboard::format::offline_frame());
    }
    tracing::info!("Symphony stopped");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_selection_follows_the_flags() {
        assert_eq!(
            store_config(&DbChoice::Disabled).unwrap(),
            StoreConfig {
                location: StoreLocation::Disabled,
                retention: None
            }
        );
        assert_eq!(
            store_config(&DbChoice::Path(":memory:".into()))
                .unwrap()
                .location,
            StoreLocation::Memory
        );
        assert_eq!(
            store_config(&DbChoice::Path("/var/lib/s.db".into()))
                .unwrap()
                .location,
            StoreLocation::File(PathBuf::from("/var/lib/s.db"))
        );
    }

    #[tokio::test]
    async fn disabled_and_in_memory_stores_open() {
        let cancel = CancellationToken::new();
        let store = open_store(&DbChoice::Disabled, &cancel).await.unwrap();
        assert!(!store.is_enabled());
        let store = open_store(&DbChoice::Path(":memory:".into()), &cancel)
            .await
            .unwrap();
        assert!(store.is_enabled());
        cancel.cancel();
    }

    #[tokio::test]
    async fn an_unopenable_database_is_a_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "x").unwrap();
        let path = dir.path().join("file/sub/s.db").display().to_string();
        let err = open_store(&DbChoice::Path(path), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            err.starts_with("Failed to open the run history database"),
            "{err}"
        );
        assert!(err.contains("--no-db"));
    }
}
