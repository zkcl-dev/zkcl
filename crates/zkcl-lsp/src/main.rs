use std::process::ExitCode;

fn main() -> ExitCode {
    match zkcl_lsp::run_stdio() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("zkcl-lsp: {error}");
            ExitCode::FAILURE
        }
    }
}
