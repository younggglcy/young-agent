use std::process::ExitCode;

fn main() -> ExitCode {
    match young_cli_proof_surface::run_from_env() {
        Ok(status) => ExitCode::from(status.code()),
        Err(error) => {
            eprintln!("young-agent: {error}");
            ExitCode::FAILURE
        }
    }
}
