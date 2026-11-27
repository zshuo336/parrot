// SPDX-License-Identifier: AGPL-3.0-only
use parrot_config::OrderedReceiverState;
use parrot_remote::ordered::handoff;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const USAGE: &str = "usage: parrot-receiver-handoff prepare|seal|activate DIRECTORY REALM NODE [SEAL_SHA256]\nprepare writes an invitation to stdout; seal reads it on stdin and writes a private seal to stdout and its SHA-256 to stderr; activate reads the seal on stdin and verifies the trusted source digest";

fn input() -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take((handoff::MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > handoff::MAX_BYTES {
        return Err("handoff document too large".into());
    }
    Ok(bytes)
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [action, directory, realm, node, rest @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let config = OrderedReceiverState {
        directory: directory.into(),
        realm: realm.clone(),
    };
    let bytes = match (action.as_str(), rest) {
        ("prepare", []) => handoff::prepare(&config, node)?,
        ("seal", []) => {
            let seal = handoff::seal(&config, node, &input()?)?;
            eprintln!("{}", hex::encode(Sha256::digest(&seal)));
            seal
        }
        ("activate", [hash]) => {
            let mut digest = [0; 32];
            hex::decode_to_slice(hash, &mut digest).map_err(|_| "invalid trusted seal SHA-256")?;
            return handoff::activate(&config, node, &input()?, digest);
        }
        _ => return Err(USAGE.into()),
    };
    std::io::stdout()
        .lock()
        .write_all(&bytes)
        .map_err(|e| e.to_string())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(detail) => {
            eprintln!("{detail}");
            std::process::ExitCode::FAILURE
        }
    }
}
