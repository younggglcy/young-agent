#![doc = "Canonical Event Log storage boundary for Agent Runs."]

pub mod jsonl;
pub mod replay;

pub use jsonl::{EventStoreError, JsonlEventStore};
pub use replay::{
    replay_events, replay_events_for_recovery, replay_events_with_compatibility,
    ReplayCompatibility, ReplayError, ReplayedToolCall, RunReplay,
};

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-event-store");
    }
}
