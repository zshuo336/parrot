// SPDX-License-Identifier: AGPL-3.0-only
//! Out-of-guest custody for a trusted, cold-boot-only QEMU q35 guest.
use super::{enroll, error, handoff, journal, peer, peer_pidfd, process_identity, receive, send};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            net::UnixStream,
        },
    },
    path::Path,
};
use uuid::Uuid;

struct Monitor(BufReader<UnixStream>);

impl Monitor {
    fn message(&mut self) -> Result<Value, String> {
        let mut line = Vec::new();
        self.0
            .by_ref()
            .take(65537)
            .read_until(b'\n', &mut line)
            .map_err(error)?;
        if line.len() > 65536 || line.last() != Some(&b'\n') {
            return Err("invalid QMP message length".into());
        }
        serde_json::from_slice(&line).map_err(error)
    }

    fn command(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let stream = self.0.get_mut();
        serde_json::to_writer(
            &mut *stream,
            &json!({"execute": name, "arguments": arguments}),
        )
        .map_err(error)?;
        stream.write_all(b"\n").map_err(error)?;
        for _ in 0..64 {
            let mut result = self.message()?;
            if let Some(reply) = result.get_mut("return") {
                return Ok(reply.take());
            }
            if result.get("event").is_none() {
                return Err(format!("QMP command {name} failed"));
            }
        }
        Err("too many QMP events during enrollment".into())
    }

    fn verify(&mut self, guest: Uuid) -> Result<(), String> {
        if self.message()?.get("QMP").is_none() {
            return Err("missing QMP greeting".into());
        }
        self.command("qmp_capabilities", json!({}))?;
        let id = self.command("query-uuid", json!({}))?;
        if id["UUID"].as_str().and_then(|s| Uuid::parse_str(s).ok()) != Some(guest) {
            return Err("wrong QEMU guest UUID".into());
        }
        // q35's PCIe root bus cannot hotplug this device. Its peer mode is
        // fixed at realization and blocks saving or migrating running memory.
        for (property, expected) in [
            ("type", "ivshmem-plain"),
            ("parent_bus", "/machine/q35/pcie.0"),
            ("master", "off"),
        ] {
            let actual = self.command(
                "qom-get",
                json!({"path": "/machine/peripheral/guard", "property": property}),
            )?;
            if actual.as_str() != Some(expected) {
                return Err(format!("QEMU migration guard {property} differs"));
            }
        }
        let migration = self.command("query-migrate", json!({}))?;
        let reason = "Migration is disabled when using feature 'peer mode' in device 'ivshmem'";
        if !migration["blocked-reasons"]
            .as_array()
            .is_some_and(|r| r.iter().any(|v| v.as_str() == Some(reason)))
        {
            return Err("QEMU has no active peer-mode migration guard".into());
        }
        Ok(())
    }
}

/// Enroll through two direct Unix sockets of the same trusted QEMU process.
/// Pending and released registrations can be replayed with the original journal,
/// source-bound invitation and guest UUID even after those sockets disappear.
pub fn qemu_witness(
    invitation: &[u8],
    directory: &Path,
    guest: Uuid,
    monitor: &Path,
    serial: &Path,
    executable: &Path,
) -> Result<Vec<u8>, String> {
    handoff::require_bound_invitation(invitation)?;
    if guest.is_nil() {
        return Err("expected a nonzero guest UUID".into());
    }
    journal::observe(invitation, directory, Some(*guest.as_bytes()), |save| {
        let control = UnixStream::connect(monitor).map_err(error)?;
        let pid = peer(&control)?.pid;
        let owner = peer_pidfd(&control)?;
        process_identity(pid)?;
        let actual = fs::metadata(format!("/proc/{pid}/exe")).map_err(error)?;
        let expected = fs::metadata(executable).map_err(error)?;
        if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
            return Err(
                "management socket does not belong to the configured QEMU executable".into(),
            );
        }
        let mut monitor = Monitor(BufReader::new(control));
        monitor.verify(guest)?;
        let mut uart = UnixStream::connect(serial).map_err(error)?;
        if peer(&uart)?.pid != pid {
            return Err("management and serial sockets belong to different processes".into());
        }
        // Request a fresh reply from the pinned monitor after inspecting the
        // second socket to exclude PID recycling between the two observations.
        monitor.command("query-status", json!({}))?;
        uart.write_all(guest.as_bytes())
            .and_then(|_| uart.write_all(&Sha256::digest(invitation)))
            .map_err(error)?;
        let proof = receive(&mut uart)?;
        handoff::verify_standby(invitation, &proof)?;
        save(pid, &proof)?;
        let _ = uart.write_all(b"R");
        Ok((proof, owner))
    })
}

/// Bridge one local receiver enrollment to its dedicated management UART. The
/// receiver gets its acknowledgment only after the external journal is durable.
pub fn enroll_guest(invitation: &[u8], device: &Path) -> Result<(), String> {
    handoff::require_bound_invitation(invitation)?;
    let id = fs::read_to_string("/sys/class/dmi/id/product_uuid").map_err(error)?;
    let guest = Uuid::parse_str(id.trim()).map_err(error)?;
    let mut uart = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(device)
        .map_err(error)?;
    let fd = uart.as_raw_fd();
    let mut settings = std::mem::MaybeUninit::uninit();
    // tcgetattr initializes the termios only on success.
    if unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) } != 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    let mut settings = unsafe { settings.assume_init() };
    unsafe { libc::cfmakeraw(&mut settings) };
    settings.c_cc[libc::VMIN] = 0;
    settings.c_cc[libc::VTIME] = 100;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &settings) } != 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    enroll(invitation, |_, proof| {
        let mut expected = [0; 48];
        uart.read_exact(&mut expected).map_err(error)?;
        if expected[..16] != guest.as_bytes()[..]
            || expected[16..] != Sha256::digest(invitation)[..]
        {
            return Err("guest UUID or target invitation differs".into());
        }
        send(&mut uart, proof)?;
        let mut ack = [0; 1];
        uart.read_exact(&mut ack).map_err(error)?;
        if ack != *b"R" {
            return Err("external witness did not acknowledge custody".into());
        }
        Ok(())
    })?;
    Ok(())
}
