#![doc = "Tool definition, policy, and execution boundary for the Agent Kernel."]

pub mod execution;
pub mod registry;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-tool-runtime");
    }
}
