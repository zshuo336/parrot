//! An application binary with its own Props factory and codec, using the real
//! shared CLI. First `capture new.jsonl`, then `replay --manifest ... --input
//! new.jsonl --report new-report.json --target-node capture-server`.
#[path = "support/replay_counter.rs"]
mod counter;
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = if args.first().and_then(|a| a.to_str()) == Some("capture") {
        if args.len() != 2 {
            Err("capture requires one new file path".into())
        } else {
            counter::capture(std::path::PathBuf::from(&args[1])).await
        }
    } else {
        parrot_app::cli::execute(args).await
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("[replay-example] {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
