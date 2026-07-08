#![doc = "Provider-neutral model runtime boundary for the Agent Kernel."]

pub mod client;
pub mod stream;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-model-runtime");
    }
}
