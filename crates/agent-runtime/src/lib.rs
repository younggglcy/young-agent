#![doc = "Agent Run orchestration boundary for the Agent Kernel."]

pub mod run;
pub mod turn;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-agent-runtime");
    }
}
