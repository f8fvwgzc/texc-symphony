//! The `symphony` binary as a library: command-line evaluation, process bootstrap, logging, the
//! terminal status dashboard, the HTTP control-plane adapter and the `workspace before-remove`
//! subcommand. `main.rs` only calls [`app::main`].
//!
//! - [`cli`]: argument and environment evaluation (exact Elixir usage / banner texts).
//! - [`app`]: startup order, signal handling and graceful shutdown.
//! - [`logging`] / [`rotating`]: the 10 MiB × 5 rotating log file plus the stdout stream.
//! - [`dashboard`]: the ANSI terminal dashboard (golden-fixture exact frames).
//! - [`control`]: `symphony_server::ControlPlane` over `symphony_runtime::RuntimeHandle`.
//! - [`memory_seed`]: `tracker.provider.issues` for the memory tracker.
//! - [`before_remove`]: closes a workspace branch's open PRs from the `before_remove` hook.
//! - [`version`]: the reported version (with the optional `-nightly` suffix).

#![warn(missing_docs)]

pub mod app;
pub mod before_remove;
pub mod cli;
pub mod control;
pub mod dashboard;
pub mod logging;
pub mod memory_seed;
pub mod rotating;
pub mod version;
