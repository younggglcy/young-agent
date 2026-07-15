#![doc = "Built-in Coding Capability boundary for the Agent Kernel."]

mod command;
mod command_input;
mod command_policy;
mod git_environment;
pub mod manifest;
mod patch;
mod read;
mod search;
mod tool_support;
pub mod tools;
pub mod workspace;

pub use command_policy::{CommandApprovalPolicy, CommandPolicyDecision};
pub use manifest::{coding_manifest, CODING_CAPABILITY_MANIFEST_TOML};
pub use tools::{register_builtin_coding_capability, CodingCapabilityRegistrationError};
pub use workspace::{CodingWorkspace, CodingWorkspaceError, GitWorktreeContext, WorkspaceContext};

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-capability-coding");
    }
}
