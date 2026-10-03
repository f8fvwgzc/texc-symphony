//! symphony-core: the pure, network-free heart of Symphony.
//!
//! - [`workflow`]: `WORKFLOW.md` loading (YAML front matter + Liquid prompt body).
//! - [`workflow_store`]: last-known-good cache with stamp-based (1 s) reload.
//! - [`config`]: Ecto-compatible casting of the front matter into typed [`config::Settings`],
//!   `$VAR` resolution, per-tracker preflight validation, sandbox-policy helpers.
//! - [`issue`]: the normalized tracker [`issue::Issue`] plus normalization helpers shared by adapters.
//! - [`prompt`]: strict Liquid prompt rendering and continuation guidance.
//! - [`path_safety`]: Elixir `Path.expand` semantics and symlink-resolving canonicalization.
//! - [`workspace_key`]: the per-issue workspace directory name.
//! - [`error`]: typed errors whose `Display` keeps the Elixir snake_case reason tags.

#![warn(missing_docs)]

pub mod config;
pub mod env;
pub mod error;
pub mod issue;
pub mod path_safety;
pub mod prompt;
pub mod workflow;
pub mod workflow_store;
pub mod workspace_key;

pub use config::Settings;
pub use env::{EnvSource, MapEnv, ProcessEnv};
pub use error::{ConfigError, IoReason, TrackerConfigError};
pub use issue::{BlockerRef, Issue};
pub use path_safety::PathError;
pub use prompt::PromptError;
pub use workflow::LoadedWorkflow;
pub use workflow_store::{WorkflowSnapshot, WorkflowStore};
pub use workspace_key::workspace_key;
