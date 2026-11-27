// SPDX-License-Identifier: AGPL-3.0-only
use parrot_config::OrderedReceiverState;
use parrot_remote::ordered::handoff;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const USAGE: &str = "usage: parrot-receiver-handoff prepare DIRECTORY REALM NODE [SOURCE_RECEIVER_HEX]\n       parrot-receiver-handoff seal|activate DIRECTORY REALM NODE [SEAL_SHA256]\nLinux: watch DIRECTORY REALM NODE, or witness [PRIVATE_JOURNAL_DIRECTORY] < invitation.json\nQEMU guest: enroll-guest UART < bound-invitation.json\nQEMU host: witness-qemu PRIVATE_JOURNAL_DIRECTORY GUEST_UUID QMP_SOCKET UART_SOCKET QEMU_EXECUTABLE < bound-invitation.json\nprepare writes an invitation to stdout; seal and witness commands write a private seal to stdout and its SHA-256 to stderr; activate checks the trusted digest before publishing";

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
    #[cfg(target_os = "linux")]
    if let [action, rest @ ..] = args.as_slice() {
        let seal = match (action.as_str(), rest) {
            ("enroll-guest", [uart]) => {
                return parrot_remote::ordered::standby::enroll_guest(&input()?, uart.as_ref())
            }
            ("witness-qemu", [directory, uuid, monitor, serial, executable]) => {
                Some(parrot_remote::ordered::standby::qemu_witness(
                    &input()?,
                    directory.as_ref(),
                    uuid::Uuid::parse_str(uuid).map_err(|_| "invalid guest UUID")?,
                    monitor.as_ref(),
                    serial.as_ref(),
                    executable.as_ref(),
                )?)
            }
            ("witness", []) => Some(parrot_remote::ordered::standby::witness(&input()?)?),
            ("witness", [directory]) => Some(parrot_remote::ordered::standby::durable_witness(
                &input()?,
                directory.as_ref(),
            )?),
            _ => None,
        };
        if let Some(seal) = seal {
            eprintln!("{}", hex::encode(Sha256::digest(&seal)));
            return std::io::stdout()
                .lock()
                .write_all(&seal)
                .map_err(|e| e.to_string());
        }
    }
    let [action, directory, realm, node, rest @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let config = OrderedReceiverState {
        directory: directory.into(),
        realm: realm.clone(),
    };
    let bytes = match (action.as_str(), rest) {
        #[cfg(target_os = "linux")]
        ("watch", []) => return parrot_remote::ordered::standby::watch(&config, node),
        ("prepare", []) => handoff::prepare(&config, node)?,
        ("prepare", [source]) => {
            let mut receiver = [0; 16];
            hex::decode_to_slice(source, &mut receiver)
                .map_err(|_| "invalid source receiver hex")?;
            handoff::prepare_from(&config, node, receiver)?
        }
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
