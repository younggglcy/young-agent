#![doc = "Minimal Rust CLI Proof Surface for the Agent Kernel."]

mod app;
mod approval;
mod args;
mod fake_provider;
mod signals;
mod state;
mod terminal;

pub use app::{run_from_env, CliError, CliExitStatus};
