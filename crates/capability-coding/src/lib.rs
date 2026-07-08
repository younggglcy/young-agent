#![doc = "Built-in Coding Capability boundary for the Agent Kernel."]

pub mod manifest;
pub mod tools;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-capability-coding");
    }
}
