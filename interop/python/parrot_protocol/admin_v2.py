"""admin-v2 wire 编解码（DEV_09 B1/B3——与 Rust `admin_v2.rs` 逐字节对齐）。

payload 布局：`[u8 tag][bincode standard config(body)]`
  tag 0x03 = AdminCommandV2 / 0x04 = AdminReplyV2

bincode standard varint（Python 侧重实现——经 docs/vectors/admin_v2.json
冻结向量逐字节验证）：
  值 ≤ 250          → 单字节
  0xFB + u16 LE     （251 ..= 65535）
  0xFC + u32 LE     （65536 ..= 2^32-1）
  0xFD + u64 LE     （更大）
serde 形态：
  - enum：externally tagged = [varint 变体索引][各字段顺序体]
  - Option<T>：0x00/0x01 + T
  - String/Vec<T>：varint len + 元素
"""

from __future__ import annotations

import struct

TAG_ADMIN_CMD_V2 = 0x03
TAG_ADMIN_REPLY_V2 = 0x04
METRICS_ALL = (1 << 17) - 1
METRICS_PROCESSES = 1 << 14

# v2 错误码扩展段（0x0A00+）
ERR_ARTIFACT_FETCH = 0x0A00
ERR_ARTIFACT_DIGEST = 0x0A01
ERR_DIALECT_MISMATCH = 0x0A02
ERR_COMPONENT_NOT_FOUND = 0x0A03
ERR_DRAIN_TIMEOUT = 0x0A04
ERR_FACTORY_NOT_FOUND = 0x0A05
ERR_SPAWN_FAILED = 0x0A06


# ---------------- varint ----------------


def put_varint(v: int, *, bits: int = 64) -> bytes:
    if type(v) is not int or not 0 <= v < (1 << bits):
        raise ValueError(f"expected unsigned {bits}-bit integer")
    if v <= 250:
        return bytes([v])
    if v <= 0xFFFF:
        return b"\xfb" + struct.pack("<H", v)
    if v <= 0xFFFFFFFF:
        return b"\xfc" + struct.pack("<I", v)
    return b"\xfd" + struct.pack("<Q", v)


def _require(buf: memoryview, off: int, size: int) -> None:
    if off < 0 or size < 0 or off > len(buf) or size > len(buf) - off:
        raise ValueError("truncated admin payload")


def read_varint(buf: memoryview, off: int, *, bits: int = 64) -> tuple[int, int]:
    _require(buf, off, 1)
    b0 = buf[off]
    if b0 <= 0xFA:
        return b0, off + 1
    formats = {0xFB: ("<H", 2), 0xFC: ("<I", 4), 0xFD: ("<Q", 8)}
    if b0 not in formats:
        raise ValueError(f"bad varint prefix 0x{b0:02x}")
    fmt, size = formats[b0]
    if size * 8 > bits:
        raise ValueError(f"integer representation exceeds u{bits}")
    _require(buf, off + 1, size)
    return struct.unpack_from(fmt, buf, off + 1)[0], off + 1 + size


def _read_option_tag(buf: memoryview, off: int) -> tuple[bool, int]:
    _require(buf, off, 1)
    if buf[off] not in (0, 1):
        raise ValueError("invalid Option discriminant")
    return buf[off] == 1, off + 1


def _read_opt_str(buf: memoryview, off: int) -> tuple[str | None, int]:
    present, off = _read_option_tag(buf, off)
    return read_str(buf, off) if present else (None, off)


def _finish(result: dict, buf: memoryview, off: int) -> dict:
    if off != len(buf):
        raise ValueError("trailing admin payload bytes")
    return result


# ---------------- 基础 serde 形态 ----------------


def put_str(s: str) -> bytes:
    b = s.encode()
    return put_varint(len(b)) + b


def read_str(buf: memoryview, off: int) -> tuple[str, int]:
    n, off = read_varint(buf, off)
    _require(buf, off, n)
    s = bytes(buf[off : off + n]).decode()
    return s, off + n


def put_opt_bytes(b: bytes | None) -> bytes:
    if b is None:
        return b"\x00"
    return b"\x01" + put_varint(len(b)) + b


def read_opt_bytes(buf: memoryview, off: int) -> tuple[bytes | None, int]:
    present, off = _read_option_tag(buf, off)
    if not present:
        return None, off
    n, off = read_varint(buf, off)
    _require(buf, off, n)
    return bytes(buf[off : off + n]), off + n


# ---------------- AdminArtifactRef ----------------
# 变体索引（externally tagged；与 Rust enum 声明顺序一致）：
#   0 Props{factory}  1 Beam{app}  2 PyModule{module, runtime_env}
#   3 Jvm{main_class, coords}  4 Wasm{digest, uri}  5 Dylib{digest, uri, abi}


def put_artifact(a: dict) -> bytes:
    kind = a["kind"]
    if kind == "props":
        return b"\x00" + put_str(a["factory"])
    if kind == "beam":
        uri = a.get("uri")
        return b"\x01" + put_str(a["app"]) + (
            b"\x00" if uri is None else b"\x01" + put_str(uri)
        )
    if kind == "pymodule":
        env = a.get("runtime_env")
        uri = a.get("uri")
        return b"\x02" + put_str(a["module"]) + (
            b"\x00" if env is None else b"\x01" + put_str(env)
        ) + (
            b"\x00" if uri is None else b"\x01" + put_str(uri)
        )
    if kind == "jvm":
        coords = a.get("coords")
        uri = a.get("uri")
        return b"\x03" + put_str(a["main_class"]) + (
            b"\x00" if coords is None else b"\x01" + put_str(coords)
        ) + (
            b"\x00" if uri is None else b"\x01" + put_str(uri)
        )
    if kind == "wasm":
        return b"\x04" + put_str(a["digest"]) + put_str(a["uri"])
    if kind == "dylib":
        return b"\x05" + put_str(a["digest"]) + put_str(a["uri"]) + put_varint(a["abi"], bits=32)
    raise ValueError(f"unknown artifact kind {kind}")


def read_artifact(buf: memoryview, off: int) -> tuple[dict, int]:
    v, off = read_varint(buf, off)
    if v == 0:
        f, off = read_str(buf, off)
        return {"kind": "props", "factory": f}, off
    if v == 1:
        app, off = read_str(buf, off)
        uri, off = _read_opt_str(buf, off)
        return {"kind": "beam", "app": app, "uri": uri}, off
    if v == 2:
        m, off = read_str(buf, off)
        env, off = _read_opt_str(buf, off)
        uri, off = _read_opt_str(buf, off)
        return {"kind": "pymodule", "module": m, "runtime_env": env, "uri": uri}, off
    if v == 3:
        mc, off = read_str(buf, off)
        coords, off = _read_opt_str(buf, off)
        uri, off = _read_opt_str(buf, off)
        return {"kind": "jvm", "main_class": mc, "coords": coords, "uri": uri}, off
    if v == 4:
        d, off = read_str(buf, off)
        u, off = read_str(buf, off)
        return {"kind": "wasm", "digest": d, "uri": u}, off
    if v == 5:
        d, off = read_str(buf, off)
        u, off = read_str(buf, off)
        abi, off = read_varint(buf, off, bits=32)
        return {"kind": "dylib", "digest": d, "uri": u, "abi": abi}, off
    raise ValueError(f"unknown artifact variant {v}")


# ---------------- AdminInstancePolicy ----------------
# 0 Singleton  1 Pool{count}  2 Sharded{count}


def put_policy(p: dict) -> bytes:
    kind = p["kind"]
    if kind == "singleton":
        return b"\x00"
    if kind == "pool":
        return b"\x01" + put_varint(p["count"])
    if kind == "sharded":
        return b"\x02" + put_varint(p["count"])
    raise ValueError(f"unknown policy {kind}")


def read_policy(buf: memoryview, off: int) -> tuple[dict, int]:
    v, off = read_varint(buf, off)
    if v == 0:
        return {"kind": "singleton"}, off
    if v == 1:
        n, off = read_varint(buf, off)
        return {"kind": "pool", "count": n}, off
    if v == 2:
        n, off = read_varint(buf, off)
        return {"kind": "sharded", "count": n}, off
    raise ValueError(f"unknown policy variant {v}")


# ---------------- ComponentDeploy ----------------


def put_component_deploy(c: dict) -> bytes:
    return (
        put_str(c["name"])
        + put_str(c["version"])
        + put_artifact(c["artifact"])
        + put_policy(c["instances"])
        + put_opt_bytes(c.get("config"))
    )


def read_component_deploy(buf: memoryview, off: int) -> tuple[dict, int]:
    name, off = read_str(buf, off)
    version, off = read_str(buf, off)
    artifact, off = read_artifact(buf, off)
    instances, off = read_policy(buf, off)
    config, off = read_opt_bytes(buf, off)
    return {
        "name": name,
        "version": version,
        "artifact": artifact,
        "instances": instances,
        "config": config,
    }, off


# ---------------- AdminCommandV2 ----------------
# 0 Deploy{req_id, component}  1 Drain{req_id, path_prefix, timeout_ms}
# 2 Stop{req_id, path_prefix}  3 Status{req_id, path_prefix}


def encode_admin_cmd_v2(cmd: dict) -> bytes:
    kind = cmd["kind"]
    if kind == "deploy":
        body = b"\x00" + put_varint(cmd["req_id"]) + put_component_deploy(cmd["component"])
    elif kind == "drain":
        body = (
            b"\x01"
            + put_varint(cmd["req_id"])
            + put_str(cmd["path_prefix"])
            + put_varint(cmd["timeout_ms"])
        )
    elif kind == "stop":
        body = b"\x02" + put_varint(cmd["req_id"]) + put_str(cmd["path_prefix"])
    elif kind == "status":
        body = b"\x03" + put_varint(cmd["req_id"]) + put_str(cmd["path_prefix"])
    elif kind == "metrics":
        body = b"\x04" + put_varint(cmd["req_id"])
    elif kind == "measured_metrics":
        body = b"\x05" + put_varint(cmd["req_id"])
    else:
        raise ValueError(f"unknown cmd kind {kind}")
    return bytes([TAG_ADMIN_CMD_V2]) + body


def decode_admin_cmd_v2(payload: bytes) -> dict:
    buf = memoryview(payload)
    _require(buf, 0, 1)
    if buf[0] != TAG_ADMIN_CMD_V2:
        raise ValueError(f"bad tag 0x{buf[0]:02x} (expect 0x03)")
    v, off = read_varint(buf, 1)
    if v == 0:
        req_id, off = read_varint(buf, off)
        comp, off = read_component_deploy(buf, off)
        return _finish({"kind": "deploy", "req_id": req_id, "component": comp}, buf, off)
    if v == 1:
        req_id, off = read_varint(buf, off)
        prefix, off = read_str(buf, off)
        timeout_ms, off = read_varint(buf, off)
        return _finish({"kind": "drain", "req_id": req_id, "path_prefix": prefix, "timeout_ms": timeout_ms}, buf, off)
    if v == 2:
        req_id, off = read_varint(buf, off)
        prefix, off = read_str(buf, off)
        return _finish({"kind": "stop", "req_id": req_id, "path_prefix": prefix}, buf, off)
    if v == 3:
        req_id, off = read_varint(buf, off)
        prefix, off = read_str(buf, off)
        return _finish({"kind": "status", "req_id": req_id, "path_prefix": prefix}, buf, off)
    if v == 4:
        req_id, off = read_varint(buf, off)
        return _finish({"kind": "metrics", "req_id": req_id}, buf, off)
    if v == 5:
        req_id, off = read_varint(buf, off)
        return _finish({"kind": "measured_metrics", "req_id": req_id}, buf, off)
    raise ValueError(f"unknown cmd variant {v}")


# ---------------- AdminReplyV2 ----------------
# 0 Deployed{req_id, instances}  1 Drained{req_id, drained, aborted}
# 2 Stopped{req_id}  3 Status{req_id, states}  4 Failed{req_id, code, detail}


def encode_admin_reply_v2(r: dict) -> bytes:
    kind = r["kind"]
    if kind == "deployed":
        body = b"\x00" + put_varint(r["req_id"])
        body += put_varint(len(r["instances"]))
        for inst in r["instances"]:
            body += put_str(inst)
    elif kind == "drained":
        body = (
            b"\x01"
            + put_varint(r["req_id"])
            + put_varint(r["drained"])
            + put_varint(r["aborted"])
        )
    elif kind == "stopped":
        body = b"\x02" + put_varint(r["req_id"])
    elif kind == "status":
        body = b"\x03" + put_varint(r["req_id"])
        body += put_varint(len(r["states"]))
        for s in r["states"]:
            body += put_str(s["path"]) + put_str(s["state"]) + put_str(s["version"])
    elif kind == "failed":
        body = (
            b"\x04"
            + put_varint(r["req_id"])
            + put_varint(r["code"], bits=16)
            + put_str(r["detail"])
        )
    elif kind == "metrics":
        body = b"\x05" + put_varint(r["req_id"]) + _put_snapshot(r["snapshot"])
    elif kind == "measured_metrics":
        metrics = r["metrics"]
        validate_measured_metrics(metrics)
        body = (b"\x06" + put_varint(r["req_id"]) + _put_snapshot(metrics["snapshot"])
                + put_varint(metrics["available"]) + put_str(metrics["epoch"])
                + put_str(metrics["process_kind"]))
    else:
        raise ValueError(f"unknown reply kind {kind}")
    return bytes([TAG_ADMIN_REPLY_V2]) + body


# ---- 观测五件套：MetricsSnapshot 编解码（字段序 = Rust 声明序）----
# ts_ms, runtime, connections, handshakes_ok, handshakes_failed,
# asks_rx, tells_rx, replies_tx, reply_errs, bytes_rx, bytes_tx,
# heartbeats_rx, components, [n, {path,state,version}...],
# processes, memory_rss, uptime_start_ms


def _put_snapshot(s: dict) -> bytes:
    out = put_varint(s.get("ts_ms", 0))
    out += put_str(str(s.get("runtime", "")))
    for f in (
        "connections", "handshakes_ok", "handshakes_failed",
        "asks_rx", "tells_rx", "replies_tx", "reply_errs",
        "bytes_rx", "bytes_tx", "heartbeats_rx",
    ):
        out += put_varint(s.get(f, 0))
    states = list(s.get("component_states", []))
    out += put_varint(s.get("components", len(states)))  # components are not instance states
    out += put_varint(len(states))  # component_states: Vec len（serde 双字段）
    for st in states:
        out += put_str(st["path"]) + put_str(st["state"]) + put_str(st["version"])
    for f in ("processes", "memory_rss", "uptime_start_ms"):
        out += put_varint(s.get(f, 0))
    return out


def _read_snapshot(buf: memoryview, off: int) -> tuple[dict, int]:
    snap: dict = {}
    snap["ts_ms"], off = read_varint(buf, off)
    snap["runtime"], off = read_str(buf, off)
    for f in (
        "connections", "handshakes_ok", "handshakes_failed",
        "asks_rx", "tells_rx", "replies_tx", "reply_errs",
        "bytes_rx", "bytes_tx", "heartbeats_rx",
    ):
        snap[f], off = read_varint(buf, off)
    comps, off = read_varint(buf, off)
    n, off = read_varint(buf, off)
    states = []
    for _ in range(n):
        path, off = read_str(buf, off)
        state, off = read_str(buf, off)
        version, off = read_str(buf, off)
        states.append({"path": path, "state": state, "version": version})
    snap["components"] = comps
    snap["component_states"] = states
    for f in ("processes", "memory_rss", "uptime_start_ms"):
        snap[f], off = read_varint(buf, off)
    return snap, off


def decode_admin_reply_v2(payload: bytes) -> dict:
    buf = memoryview(payload)
    _require(buf, 0, 1)
    if buf[0] != TAG_ADMIN_REPLY_V2:
        raise ValueError(f"bad tag 0x{buf[0]:02x} (expect 0x04)")
    v, off = read_varint(buf, 1)
    if v == 0:
        req_id, off = read_varint(buf, off)
        n, off = read_varint(buf, off)
        instances = []
        for _ in range(n):
            inst, off = read_str(buf, off)
            instances.append(inst)
        return _finish({"kind": "deployed", "req_id": req_id, "instances": instances}, buf, off)
    if v == 1:
        req_id, off = read_varint(buf, off)
        drained, off = read_varint(buf, off)
        aborted, off = read_varint(buf, off)
        return _finish({"kind": "drained", "req_id": req_id, "drained": drained, "aborted": aborted}, buf, off)
    if v == 2:
        req_id, off = read_varint(buf, off)
        return _finish({"kind": "stopped", "req_id": req_id}, buf, off)
    if v == 3:
        req_id, off = read_varint(buf, off)
        n, off = read_varint(buf, off)
        states = []
        for _ in range(n):
            path, off = read_str(buf, off)
            state, off = read_str(buf, off)
            version, off = read_str(buf, off)
            states.append({"path": path, "state": state, "version": version})
        return _finish({"kind": "status", "req_id": req_id, "states": states}, buf, off)
    if v == 4:
        req_id, off = read_varint(buf, off)
        code, off = read_varint(buf, off, bits=16)
        detail, off = read_str(buf, off)
        return _finish({"kind": "failed", "req_id": req_id, "code": code, "detail": detail}, buf, off)
    if v == 5:
        req_id, off = read_varint(buf, off)
        snap, off = _read_snapshot(buf, off)
        return _finish({"kind": "metrics", "req_id": req_id, "snapshot": snap}, buf, off)
    if v == 6:
        req_id, off = read_varint(buf, off)
        snapshot, off = _read_snapshot(buf, off)
        available, off = read_varint(buf, off)
        epoch, off = read_str(buf, off)
        process_kind, off = read_str(buf, off)
        metrics = {"snapshot": snapshot, "available": available,
                   "epoch": epoch, "process_kind": process_kind}
        validate_measured_metrics(metrics)
        return _finish({"kind": "measured_metrics", "req_id": req_id, "metrics": metrics}, buf, off)
    raise ValueError(f"unknown reply variant {v}")


def decode_tag(payload: bytes) -> int:
    """SYSTEM_EVENT payload 首字节（0x03/0x04 → admin v2；其它 → ValueError）。"""
    t = payload[0] if payload else -1
    if t in (TAG_ADMIN_CMD_V2, TAG_ADMIN_REPLY_V2):
        return t
    raise ValueError(f"not admin-v2 payload (tag=0x{t:02x})")


def validate_measured_metrics(metrics: dict) -> None:
    available = metrics["available"]
    if type(available) is not int or available < 0 or available & ~METRICS_ALL:
        raise ValueError("unknown metrics availability bit")
    for field in ("epoch", "process_kind"):
        if not isinstance(metrics[field], str) or len(metrics[field].encode()) > 128:
            raise ValueError("invalid metrics identity")
    if available & METRICS_PROCESSES and not metrics["process_kind"]:
        raise ValueError("missing process metric unit")
