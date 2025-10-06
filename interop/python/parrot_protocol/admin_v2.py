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

# v2 错误码扩展段（0x0A00+）
ERR_ARTIFACT_FETCH = 0x0A00
ERR_ARTIFACT_DIGEST = 0x0A01
ERR_DIALECT_MISMATCH = 0x0A02
ERR_COMPONENT_NOT_FOUND = 0x0A03
ERR_DRAIN_TIMEOUT = 0x0A04
ERR_FACTORY_NOT_FOUND = 0x0A05
ERR_SPAWN_FAILED = 0x0A06


# ---------------- varint ----------------


def put_varint(v: int) -> bytes:
    if v <= 250:
        return bytes([v])
    if v <= 0xFFFF:
        return b"\xfb" + struct.pack("<H", v)
    if v <= 0xFFFFFFFF:
        return b"\xfc" + struct.pack("<I", v)
    return b"\xfd" + struct.pack("<Q", v)


def read_varint(buf: memoryview, off: int) -> tuple[int, int]:
    b0 = buf[off]
    if b0 <= 0xFA:
        return b0, off + 1
    if b0 == 0xFB:
        return struct.unpack_from("<H", buf, off + 1)[0], off + 3
    if b0 == 0xFC:
        return struct.unpack_from("<I", buf, off + 1)[0], off + 5
    if b0 == 0xFD:
        return struct.unpack_from("<Q", buf, off + 1)[0], off + 9
    raise ValueError(f"bad varint prefix 0x{b0:02x} (0xFE/0xFF reserved)")


# ---------------- 基础 serde 形态 ----------------


def put_str(s: str) -> bytes:
    b = s.encode()
    return put_varint(len(b)) + b


def read_str(buf: memoryview, off: int) -> tuple[str, int]:
    n, off = read_varint(buf, off)
    s = bytes(buf[off : off + n]).decode()
    return s, off + n


def put_opt_bytes(b: bytes | None) -> bytes:
    if b is None:
        return b"\x00"
    return b"\x01" + put_varint(len(b)) + b


def read_opt_bytes(buf: memoryview, off: int) -> tuple[bytes | None, int]:
    tag = buf[off]
    off += 1
    if tag == 0:
        return None, off
    n, off = read_varint(buf, off)
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
        return b"\x05" + put_str(a["digest"]) + put_str(a["uri"]) + put_varint(a["abi"])
    raise ValueError(f"unknown artifact kind {kind}")


def read_artifact(buf: memoryview, off: int) -> tuple[dict, int]:
    v, off = read_varint(buf, off)
    if v == 0:
        f, off = read_str(buf, off)
        return {"kind": "props", "factory": f}, off
    if v == 1:
        app, off = read_str(buf, off)
        uri = None
        if buf[off] == 1:
            uri, off = read_str(buf, off + 1)
        else:
            off += 1
        return {"kind": "beam", "app": app, "uri": uri}, off
    if v == 2:
        m, off = read_str(buf, off)
        env = None
        if buf[off] == 1:
            env, off = read_str(buf, off + 1)
        else:
            off += 1
        uri = None
        if buf[off] == 1:
            uri, off = read_str(buf, off + 1)
        else:
            off += 1
        return {"kind": "pymodule", "module": m, "runtime_env": env, "uri": uri}, off
    if v == 3:
        mc, off = read_str(buf, off)
        coords = None
        if buf[off] == 1:
            coords, off = read_str(buf, off + 1)
        else:
            off += 1
        uri = None
        if buf[off] == 1:
            uri, off = read_str(buf, off + 1)
        else:
            off += 1
        return {"kind": "jvm", "main_class": mc, "coords": coords, "uri": uri}, off
    if v == 4:
        d, off = read_str(buf, off)
        u, off = read_str(buf, off)
        return {"kind": "wasm", "digest": d, "uri": u}, off
    if v == 5:
        d, off = read_str(buf, off)
        u, off = read_str(buf, off)
        abi, off = read_varint(buf, off)
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
    else:
        raise ValueError(f"unknown cmd kind {kind}")
    return bytes([TAG_ADMIN_CMD_V2]) + body


def decode_admin_cmd_v2(payload: bytes) -> dict:
    buf = memoryview(payload)
    if buf[0] != TAG_ADMIN_CMD_V2:
        raise ValueError(f"bad tag 0x{buf[0]:02x} (expect 0x03)")
    v, off = read_varint(buf, 1)
    if v == 0:
        req_id, off = read_varint(buf, off)
        comp, off = read_component_deploy(buf, off)
        return {"kind": "deploy", "req_id": req_id, "component": comp}
    if v == 1:
        req_id, off = read_varint(buf, off)
        prefix, off = read_str(buf, off)
        timeout_ms, off = read_varint(buf, off)
        return {"kind": "drain", "req_id": req_id, "path_prefix": prefix, "timeout_ms": timeout_ms}
    if v == 2:
        req_id, off = read_varint(buf, off)
        prefix, off = read_str(buf, off)
        return {"kind": "stop", "req_id": req_id, "path_prefix": prefix}
    if v == 3:
        req_id, off = read_varint(buf, off)
        prefix, off = read_str(buf, off)
        return {"kind": "status", "req_id": req_id, "path_prefix": prefix}
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
            + put_varint(r["code"])
            + put_str(r["detail"])
        )
    else:
        raise ValueError(f"unknown reply kind {kind}")
    return bytes([TAG_ADMIN_REPLY_V2]) + body


def decode_admin_reply_v2(payload: bytes) -> dict:
    buf = memoryview(payload)
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
        return {"kind": "deployed", "req_id": req_id, "instances": instances}
    if v == 1:
        req_id, off = read_varint(buf, off)
        drained, off = read_varint(buf, off)
        aborted, off = read_varint(buf, off)
        return {"kind": "drained", "req_id": req_id, "drained": drained, "aborted": aborted}
    if v == 2:
        req_id, off = read_varint(buf, off)
        return {"kind": "stopped", "req_id": req_id}
    if v == 3:
        req_id, off = read_varint(buf, off)
        n, off = read_varint(buf, off)
        states = []
        for _ in range(n):
            path, off = read_str(buf, off)
            state, off = read_str(buf, off)
            version, off = read_str(buf, off)
            states.append({"path": path, "state": state, "version": version})
        return {"kind": "status", "req_id": req_id, "states": states}
    if v == 4:
        req_id, off = read_varint(buf, off)
        code, off = read_varint(buf, off)
        detail, off = read_str(buf, off)
        return {"kind": "failed", "req_id": req_id, "code": code, "detail": detail}
    raise ValueError(f"unknown reply variant {v}")


def decode_tag(payload: bytes) -> int:
    """SYSTEM_EVENT payload 首字节（0x03/0x04 → admin v2；其它 → ValueError）。"""
    t = payload[0] if payload else -1
    if t in (TAG_ADMIN_CMD_V2, TAG_ADMIN_REPLY_V2):
        return t
    raise ValueError(f"not admin-v2 payload (tag=0x{t:02x})")
