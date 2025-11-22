use std::process::Command;

pub fn cargo() -> Command {
    with_flags(
        std::env::var("RUSTFLAGS").ok().as_deref(),
        std::env::var("CARGO_ENCODED_RUSTFLAGS").ok().as_deref(),
    )
}

pub fn with_flags(ordinary: Option<&str>, encoded: Option<&str>) -> Command {
    // Cargo gives the encoded form precedence even when it is empty. Put the
    // unload requirement last so a caller's prefer-dynamic=no cannot undo it.
    let mut flags: Vec<&str> = match encoded {
        Some("") => Vec::new(),
        Some(value) => value.split('\x1f').collect(),
        None => ordinary.unwrap_or_default().split_whitespace().collect(),
    };
    if !flags.ends_with(&["-C", "prefer-dynamic"]) {
        flags.extend(["-C", "prefer-dynamic"]);
    }
    let mut command = Command::new("cargo");
    command.env("CARGO_ENCODED_RUSTFLAGS", flags.join("\x1f"));
    command
}
