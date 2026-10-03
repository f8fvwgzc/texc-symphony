//! symphony-codex: the Codex app-server client (`SymphonyElixir.Codex.AppServer` + `Codex.DynamicTool`).
//!
//! A session launches `codex app-server` (locally through `bash -lc`, or remotely through a
//! runtime-provided [`RemoteLauncher`]), speaks newline-delimited JSON over stdio with Elixir wire
//! parity (no `jsonrpc` field, fixed ids 1/2/3), and streams [`CodexEvent`]s to an [`EventSink`] while a
//! turn runs.
//!
//! ```no_run
//! # async fn demo(store: &symphony_core::WorkflowStore, issue: &symphony_core::Issue) -> Result<(), symphony_codex::CodexError> {
//! use symphony_codex::{AppServerSession, EventSink, StartOptions};
//! let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
//! let mut session = AppServerSession::start(StartOptions::from_store(store, "/ws/MT-1")).await?;
//! let outcome = session.run_turn("Do the work", issue, &EventSink::new(tx)).await;
//! session.stop().await;
//! while let Ok(event) = rx.try_recv() {
//!     println!("{} {:?}", event.kind().as_str(), event.token_usage);
//! }
//! outcome.map(drop)
//! # }
//! ```
//!
//! Modules:
//! - [`session`]: [`AppServerSession`] (`start` / `run_turn` / `stop`) and [`run`].
//! - [`protocol`]: request builders, response matching, approval / input-required classification.
//! - [`event`]: [`CodexEvent`], [`CodexEventKind`], [`EventSink`].
//! - [`tokens`]: token-usage and rate-limit extraction, [`TokenAccumulator`].
//! - [`dynamic_tool`]: [`DynamicToolHandler`] and result normalization.
//! - [`launch`]: workspace cwd validation, launch command construction, [`RemoteLauncher`].
//! - [`error`]: [`CodexError`] with stable snake_case tags, [`Blocker`].

#![warn(missing_docs)]

pub mod dynamic_tool;
pub mod error;
pub mod event;
pub mod launch;
pub mod protocol;
pub mod session;
pub mod tokens;
mod transport;

pub use dynamic_tool::{DynamicToolHandler, NoDynamicTools};
pub use error::{Blocker, CodexError, InvalidWorkspaceCwd};
pub use event::{CodexEvent, CodexEventData, CodexEventKind, EventSink, StreamMessage};
pub use launch::RemoteLauncher;
pub use session::{AppServerSession, DEFAULT_STOP_GRACE, StartOptions, TurnOutcome, run};
pub use tokens::{TokenAccumulator, TokenCounts, TokenUsage};
pub use transport::{MAX_STREAM_LOG_CHARS, STDERR_TAIL_LINES};
