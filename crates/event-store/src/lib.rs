#![doc = "Canonical Event Log storage boundary for Agent Runs."]

pub mod jsonl;
pub mod replay;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-event-store");
    }
}
