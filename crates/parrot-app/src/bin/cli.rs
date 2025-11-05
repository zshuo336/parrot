//! Shared application runner and replay command line.
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match parrot_app::cli::execute(std::env::args_os().skip(1)).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("[app] {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
