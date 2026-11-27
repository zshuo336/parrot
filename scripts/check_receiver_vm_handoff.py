#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Exercise receiver retirement across a power cut and cold boot of a Linux KVM guest.

Requires QEMU x86_64 with KVM and slirp, a kernel with built-in virtio-net,
virtio-blk and ext4, static BusyBox, cpio, mkfs.ext4 and openssl. Supply the
Linux parrot-receiver-handoff and ordered_receiver_recovery test executables.
Only a newly created raw disk is formatted. The control plane stays on loopback.
"""

import argparse
import contextlib
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time


def checked(args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, timeout=30, **kwargs).stdout


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def qmp(monitor, execute, **arguments):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(3)
        sock.connect(str(monitor))
        with sock.makefile("rwb", buffering=0) as channel:
            require("QMP" in json.loads(channel.readline()), "missing QMP greeting")
            for command in ({"execute": "qmp_capabilities"}, {"execute": execute,
                            "arguments": arguments}):
                channel.write(json.dumps(command).encode() + b"\n")
                response = json.loads(channel.readline())
                while "event" in response:
                    response = json.loads(channel.readline())
                if command["execute"] == "qmp_capabilities":
                    require("return" in response, repr(response))
            return response


def check_memory_state_guard(monitor, disk):
    reason = "Migration is disabled when using feature 'peer mode' in device 'ivshmem'"
    blockers = qmp(monitor, "query-migrate")["return"].get("blocked-reasons", [])
    require(reason in blockers, repr(blockers))
    migration = qmp(monitor, "migrate", uri=f"file:{disk.parent}/migration")["error"]["desc"]
    require(migration == reason, migration)
    unplug = qmp(monitor, "device_del", id="guard")["error"]["desc"]
    require("does not support hotplugging" in unplug, unplug)
    change = qmp(monitor, "qom-set", path="/machine/peripheral/guard", property="master",
                 value="on")["error"]["desc"]
    require("after it was realized" in change, change)
    job = "save-receiver"
    started = qmp(monitor, "snapshot-save", **{"job-id": job, "tag": "receiver",
                  "vmstate": "receiver-disk", "devices": ["receiver-disk"]})
    require("return" in started, repr(started))
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        jobs = qmp(monitor, "query-jobs")["return"]
        result = next((j for j in jobs if j["id"] == job and j["status"] == "concluded"), None)
        if result is not None:
            require(result.get("error") == reason, repr(result))
            return dict(migration=migration, memory_snapshot=result["error"], unplug=unplug,
                        mode_change=change)
        time.sleep(.05)
    raise RuntimeError("QEMU snapshot job did not conclude")


def probe_boot(port):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=.7)
    try:
        connection.request("GET", "/boot")
        response = connection.getresponse()
        return response.read().decode().strip() if response.status == 200 else None
    except (OSError, http.client.HTTPException):
        return None
    finally:
        connection.close()


def wait_for(proc, predicate, detail):
    deadline = time.monotonic() + 45
    while not predicate():
        if proc.poll() is not None or time.monotonic() >= deadline:
            raise RuntimeError(detail())
        time.sleep(.05)


@contextlib.contextmanager
def running(args, log, **kwargs):
    with log.open("wb") as out:
        proc = subprocess.Popen(args, stdout=out, stderr=subprocess.STDOUT, **kwargs)
        try:
            yield proc
        finally:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=10)


def certificates(directory, name):
    key = directory / (name + ".key")
    common = "recovery-target" if name == "receiver" else "recovery-source"
    request = checked(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes",
                       "-keyout", key, "-subj", "/CN=" + common, "-addext",
                       "subjectAltName=DNS:" + common])
    checked(["openssl", "x509", "-req", "-days", "1", "-CA", directory / "ca.pem",
             "-CAkey", directory / "ca.key", "-CAcreateserial", "-copy_extensions",
             "copyall", "-out", directory / (name + ".pem")], input=request)


def initramfs(args, root):
    tree = root / "tree"
    for name in ("bin", "proc", "sys", "dev", "runlab", "tls", "tmp"):
        (tree / name).mkdir(parents=True)
    for src, dest in [(args.busybox, "bin/busybox"), (args.receiver, "receiver"),
                      (args.handoff, "handoff"), (Path(__file__).resolve().parent /
                      "fixtures/receiver_vm_init.sh", "init")]:
        shutil.copyfile(src, tree / dest)
        (tree / dest).chmod(0o755)
    for binary in (args.receiver, args.handoff):
        for library in re.findall(r"(/[^\s]+) \(0x", checked(["ldd", binary]).decode()):
            target = tree / library.lstrip("/")
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(library, target)
    for item in ("ca.pem", "receiver.pem", "receiver.key"):
        shutil.copyfile(root / item, tree / "tls" / item)
    entries = [str(path.relative_to(tree)) for path in sorted(tree.rglob("*"))]
    with (root / "initrd").open("wb") as archive:
        subprocess.run(["cpio", "-o", "-H", "newc"], stdout=archive, cwd=tree,
                       input="\n".join(entries + [""]).encode(), check=True)


def exercise(args, root):
    state = root / "state"
    state.mkdir()
    params = ["recovery-contract", "recovery-target"]
    checked(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
             "-keyout", root / "ca.key", "-out", root / "ca.pem", "-subj", "/CN=vm-test",
             "-addext", "basicConstraints=critical,CA:TRUE"])
    certificates(root, "receiver")
    certificates(root, "sender")
    initramfs(args, root)
    disk = root / "receiver.raw"
    with disk.open("xb") as image:
        image.truncate(32 * 1024 * 1024)
    checked(["mkfs.ext4", "-q", "-F", disk])
    with socket.socket() as probe, socket.socket() as health:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
        health.bind(("127.0.0.1", 0))
        health_port = health.getsockname()[1]
    qemu = ["qemu-system-x86_64", "-machine", "q35,accel=kvm", "-cpu", "host", "-m", "384",
            "-smp", "2", "-no-reboot", "-nodefaults", "-display", "none",
            "-monitor", "none", "-kernel", args.kernel, "-initrd", root / "initrd",
            "-qmp", f"unix:{root}/monitor,server=on,wait=off",
            "-append", "console=ttyS0 rdinit=/init panic=-1 loglevel=3 net.ifnames=0",
            "-drive", f"id=receiver-disk,file={disk},format=raw,if=virtio",
            "-object", f"memory-backend-file,id=shm,size=1M,share=on,mem-path={root}/guard.mem",
            "-device", "ivshmem-plain,id=guard,bus=pcie.0,memdev=shm,master=off",
            "-device", "virtio-net-pci,id=guestnic,netdev=n", "-netdev",
            f"user,id=n,restrict=on,hostfwd=tcp:127.0.0.1:{port}-:8317,"
            f"hostfwd=tcp:127.0.0.1:{health_port}-:8318"]
    proof = root / "proof"
    def guest(label, admin_serial):
        return running(qemu + ["-serial", "file:" + str(root / label),
                               "-serial", admin_serial], root / (label + ".qemu"))

    def marker(proc, label, name):
        console = root / label
        def contents():
            return console.read_text(errors="replace") if console.exists() else ""
        wait_for(proc, lambda: name in contents(), lambda: contents() +
                 (root / (label + ".qemu")).read_text(errors="replace"))

    with contextlib.ExitStack() as stack:
        admin_socket = root / "enroll.sock"
        old = stack.enter_context(guest("original", f"unix:{admin_socket},server=on,wait=off"))
        marker(old, "original", "PARROT_SOURCE ")
        receiver_id = re.search(r"PARROT_SOURCE ([0-9a-f]{32})\b", (root / "original").read_text())[1]
        invite = checked([args.handoff, "prepare", state, *params, receiver_id])
        require(bytes(json.loads(invite)["source"]).hex() == receiver_id, "wrong invitation source")
        with socket.socket(socket.AF_UNIX) as admin:
            admin.connect(str(admin_socket))
            admin.sendall(invite)
            marker(old, "original", "PARROT_ENROLLED")
        senderlog = root / "sender.log"
        sender = stack.enter_context(running(
            [args.receiver, "--ignored", "--exact", "receiver_handoff::destination_host",
             "--nocapture"], senderlog, env=dict(os.environ, PARROT_HANDOFF_RUN=str(root),
                                                 PARROT_HANDOFF_OLD=f"127.0.0.1:{port}")))
        wait_for(sender, lambda: (root / "disconnected").exists(), senderlog.read_text)
        marker(old, "original", "PARROT_DELIVERY_BLOCKED")
        original_boot = re.search(r"PARROT_BOOT ([0-9a-f-]{36})", (root / "original").read_text())[1]
        require(probe_boot(health_port) == original_boot, "guest network did not return boot ID")
        monitor = root / "monitor"
        require("return" in qmp(monitor, "set_link", name="guestnic", up=False), "link down failed")
        require(probe_boot(health_port) is None, "guest still reachable with NIC down")
        require("return" in qmp(monitor, "set_link", name="guestnic", up=True), "link up failed")
        wait_for(old, lambda: probe_boot(health_port) == original_boot,
                 lambda: "guest NIC did not recover before fencing")
        denied = check_memory_state_guard(monitor, disk)
        old.send_signal(signal.SIGSTOP)
        stopped, status = os.waitpid(old.pid, os.WUNTRACED)
        require(stopped == old.pid and os.WIFSTOPPED(status), "VM did not pause")
        # A second process must fail on the disk lock, independently of port use.
        attempt = subprocess.run(["qemu-system-x86_64", "-nodefaults", "-display", "none",
                                  "-S", "-drive", f"file={disk},if=virtio,format=raw"],
                                 capture_output=True, timeout=5)
        require(attempt.returncode != 0 and b"write" in attempt.stderr.lower()
                and b"lock" in attempt.stderr.lower(), attempt.stderr.decode())
        old.send_signal(signal.SIGCONT)
        old.kill()
        require(old.wait(timeout=8) == -signal.SIGKILL, "original VM was not power cut")
        reboot = stack.enter_context(guest("reboot", "file:" + str(proof)))
        marker(reboot, "reboot", "PARROT_OFFLINE_SEAL")
        for expected in ("PARROT_REFUSED_OLD_RECEIVER", "PARROT_REFUSED_MISSING_STATE",
                         "PARROT_REFUSED_OLD_BOOT"):
            marker(reboot, "reboot", expected)
        require(reboot.wait(timeout=12) == 0, "rescue boot did not shut down cleanly")
        def boot_id(label):
            return re.search(r"PARROT_BOOT ([0-9a-f-]{36})", (root / label).read_text())[1]
        boots = [Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                 boot_id("original"), boot_id("reboot")]
        require(len(set(boots)) == 3, "host and guests require distinct kernel boots")
        seal = proof.read_bytes()
        digest = hashlib.sha256(seal).hexdigest()
        checked([args.handoff, "activate", state, *params, digest], input=seal)
        (root / "activated").touch()
        require(sender.wait(timeout=15) == 0 and "observed message 37" in senderlog.read_text(),
                senderlog.read_text())
        return dict(passed=True, boot_ids=boots, receiver_id=receiver_id, nic_partition_and_recovery=True,
                    qemu_denials=denied,
                    paused_disk_attach_rejected=True,
                    pending_witness_rejected_after_reboot=True, sealed_receiver_rejoin_rejected=True,
                    missing_state_reinitialization_rejected=True,
                    recovered_original_volume=True, application_received=37, seal_sha256=digest,
                    hypervisor=checked(["qemu-system-x86_64", "--version"]).decode().splitlines()[0],
                    disk_lock_error=attempt.stderr.decode().replace(str(root), "$RUN"),
                    original_console=(root / "original").read_text(),
                    reboot_console=(root / "reboot").read_text(),
                    destination_log=senderlog.read_text())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("kernel", "busybox", "receiver", "handoff"):
        parser.add_argument("--" + arg, required=True, type=lambda p: Path(p).resolve(strict=True))
    args = parser.parse_args()
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix="parrot-vm-") as tmp:
        print(json.dumps(exercise(args, Path(tmp)), indent=2))


if __name__ == "__main__":
    main()
