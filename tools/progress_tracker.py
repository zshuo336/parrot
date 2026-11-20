"""完整的本机项目进度系统：校验、计划同步、服务、离线导出和不可变保存点。"""
import argparse
from collections import Counter
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import shutil
import sys
from urllib.parse import unquote, urlsplit
import uuid

STATES = ("未开始", "进行中", "已实现待验证", "已验证", "受外部条件阻塞", "延期", "不支持")
HEADER = re.compile(r"^### (T\d{2}-\d{2}) (.+)$", re.M)
MAX_BYTES = 4 * 1024 * 1024
DATA_MARKER = '<script id="tracker-data" type="application/json">null</script>'
TRACKING_FILES = ("docs/TASKS.md", "docs/EXECUTION-PLAN.md", "docs/TODO-MASTER.md",
                  "docs/progress/config.json", "docs/progress/current.json", "docs/progress/scope.json")


def read_text(path):
    if path.stat().st_size > MAX_BYTES:
        raise ValueError("读取来源超过4MiB：" + path.name)
    return path.read_text(encoding="utf-8")


def read_json(path):
    value = json.loads(read_text(path))
    if not isinstance(value, dict):
        raise ValueError("JSON 顶层必须是对象：" + path.name)
    return value


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest() if hasattr(hashlib, "file_digest") else hashlib.sha256(stream.read()).hexdigest()


def within(root, relative):
    root = Path(root).resolve()
    if not isinstance(relative, str) or not relative or Path(relative).is_absolute():
        raise ValueError("要求仓库相对路径")
    path = (root / relative).resolve()
    if not path.is_relative_to(root) or path == root:
        raise ValueError("来源路径越出配置仓库：" + relative)
    return path


def sections(text):
    matches = list(HEADER.finditer(text))
    return [(match[1], match[2], text[match.end():matches[i + 1].start() if i + 1 < len(matches) else len(text)])
            for i, match in enumerate(matches)]


def parse_tasks(text):
    tasks = []
    for identity, title, body in sections(text):
        fields = {}
        for label in ("状态", "剩余人日", "依赖", "已做", "待做", "验收", "证据"):
            values = re.findall(r"^" + label + r"：([^\n]+)$", body, re.M)
            if len(values) != 1 or not values[0].strip():
                raise ValueError(identity + " 缺少或重复字段：" + label)
            fields[label] = values[0].strip()
        effort = re.fullmatch(r"(\d+)-(\d+)", fields["剩余人日"])
        if fields["状态"] not in STATES or not effort:
            raise ValueError(identity + " 状态或工作量不合法")
        low, high = map(int, effort.groups())
        if low > high or (fields["状态"] == "已验证" and high != 0) or (fields["状态"] != "已验证" and high == 0):
            raise ValueError(identity + " 工作量与完成状态不符")
        deps = [] if fields["依赖"] == "无" else [v.strip() for v in fields["依赖"].split(",")]
        if any(not re.fullmatch(r"T\d{2}-\d{2}", v) for v in deps) or len(set(deps)) != len(deps):
            raise ValueError(identity + " 依赖编号不合法或重复；使用半角逗号分隔")
        evidence = [] if fields["证据"] == "无" else [v.strip() for v in fields["证据"].split(",")]
        if any(not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_-]*", v) for v in evidence) or len(set(evidence)) != len(evidence):
            raise ValueError(identity + " 证据编号不合法或重复")
        tasks.append(dict(id=identity, title=title, status=fields["状态"], remaining_person_days=[low, high],
                          dependencies=deps, implemented=fields["已做"], remaining=fields["待做"],
                          acceptance=fields["验收"], evidence_ids=evidence, evidence=fields["证据"]))
    if not tasks or len({t["id"] for t in tasks}) != len(tasks):
        raise ValueError("任务为空或编号重复")
    by_id = {task["id"]: task for task in tasks}
    visited, visiting = set(), set()

    def visit(identity):
        if identity in visiting:
            raise ValueError(identity + " 存在依赖循环")
        if identity in visited:
            return
        visiting.add(identity)
        for dep in by_id[identity]["dependencies"]:
            if dep not in by_id:
                raise ValueError(identity + " 未知依赖：" + dep)
            if by_id[identity]["status"] == "已验证" and by_id[dep]["status"] != "已验证":
                raise ValueError(identity + " 已验证但依赖尚未退出")
            visit(dep)
        visiting.remove(identity)
        visited.add(identity)

    for identity in by_id:
        visit(identity)
    return tasks


def settings(root):
    root = Path(root).resolve()
    config = read_json(within(root, "docs/progress/config.json"))
    if config.get("schema_version") != 1 or not isinstance(config.get("project_name"), str) or not config["project_name"].strip():
        raise ValueError("配置版本或项目名不合法")
    if not isinstance(config.get("scope"), str) or not config["scope"].strip():
        raise ValueError("缺少非空范围说明")
    if type(config.get("port")) is not int or not 0 <= config["port"] <= 65535:
        raise ValueError("端口不合法")
    repositories = config.get("repositories")
    if not isinstance(repositories, dict) or not repositories:
        raise ValueError("缺少明确的仓库映射")
    roots = {}
    for key, location in repositories.items():
        if not re.fullmatch(r"[a-zA-Z0-9_-]+", key) or not isinstance(location, str) or not location:
            raise ValueError("仓库映射不合法")
        roots[key] = (root / location).resolve()
        if not roots[key].is_dir():
            raise ValueError("配置仓库不存在：" + key)
    phases = config.get("phases", {})
    if not isinstance(phases, dict) or any(not re.fullmatch(r"T\d{2}", k) or not isinstance(v, str) or not v for k, v in phases.items()):
        raise ValueError("阶段配置不合法")
    documents = {}
    bindings = config.get("documents")
    if not isinstance(bindings, dict) or "tasks" not in bindings:
        raise ValueError("文档清单必须包含tasks入口")
    for key, binding in bindings.items():
        if not re.fullmatch(r"[a-zA-Z0-9_-]+", key) or not isinstance(binding, dict) or not isinstance(binding.get("name"), str) or not binding["name"].strip():
            raise ValueError("文档编号或名称不合法")
        documents[key] = within(roots[binding["repository"]], binding["path"])
    return config, roots, documents


def task_ids(value, available, label):
    if not isinstance(value, list) or any(not isinstance(v, str) for v in value) or len(set(value)) != len(value) or not set(value).issubset(available):
        raise ValueError(label + " 包含重复或未知任务")
    return value


def load_state(root):
    root = Path(root).resolve()
    config, repositories, documents = settings(root)
    source = within(root, "docs/TASKS.md")
    text = read_text(source)
    tasks = parse_tasks(text)
    by_id = {task["id"]: task for task in tasks}
    scope = read_json(within(root, "docs/progress/scope.json"))
    if scope.get("schema_version") != 1 or not isinstance(scope.get("revision"), str) or not scope["revision"]:
        raise ValueError("范围清单版本或修订号不合法")
    active = task_ids(scope["active_task_ids"], set(by_id), "范围清单")
    if set(active) != set(by_id):
        raise ValueError("范围与总任务清单不一致")
    removed = scope.get("removed_tasks", [])
    if not isinstance(removed, list) or any(not isinstance(t, dict) or not re.fullmatch(r"T\d{2}-\d{2}", t.get("id", ""))
            or not all(isinstance(t.get(k), str) and t[k].strip() for k in ("reason", "decision", "date")) for t in removed):
        raise ValueError("移除项须有编号、理由、决定和日期")
    removed_ids = [t["id"] for t in removed]
    if len(set(removed_ids)) != len(removed_ids) or set(active).intersection(removed_ids):
        raise ValueError("已移除任务重复或仍在执行范围")
    plan = sections(read_text(within(root, "docs/EXECUTION-PLAN.md")))
    if len(plan) != len(tasks) or {part[0] for part in plan} != set(by_id):
        raise ValueError("执行计划与总任务范围不一致")
    for identity, title, body in plan:
        mirrors = re.findall(r"^状态：([^\n]+)$", body, re.M)
        if mirrors != [by_id[identity]["status"]] or title != by_id[identity]["title"]:
            raise ValueError(identity + " 计划状态或标题不同，请执行 sync-plan")
    if any(task["id"][:3] not in config["phases"] for task in tasks):
        raise ValueError("任务阶段未配置")
    for _, target in re.findall(r"\[([^\]]+)\]\(([^)]+)\)", text):
        if "://" not in target and not target.startswith("#"):
            linked = within(root, "docs/" + target.split("#", 1)[0])
            if not linked.exists():
                raise ValueError("任务链接不存在：" + target)
    notes = read_json(within(root, "docs/progress/current.json"))
    if notes.get("schema_version") != 1 or notes.get("current_task_id") not in by_id:
        raise ValueError("当前任务编号或说明版本不合法")
    for key in ("updated", "scope_change", "headline", "current_detail", "boundary"):
        if not isinstance(notes.get(key), str) or not notes[key].strip():
            raise ValueError("当前说明缺少字段：" + key)
    if not isinstance(notes.get("limits"), list) or any(not isinstance(v, str) for v in notes["limits"]):
        raise ValueError("限制说明不合法")
    queue = task_ids(notes.get("queue"), set(by_id), "执行队列")
    notes["queue"] = [dict(title=by_id[t]["title"], status=by_id[t]["status"], detail=by_id[t]["remaining"]) for t in queue]
    delivery = []
    groups = notes.get("delivery_groups")
    if not isinstance(groups, list) or any(not isinstance(group, dict) for group in groups):
        raise ValueError("交付分组必须为对象数组")
    for group in groups:
        group_ids = task_ids(group["task_ids"], set(by_id), "交付分组")
        if not group_ids or not isinstance(group.get("name"), str) or not group["name"]:
            raise ValueError("交付分组不能为空")
        done = sum(by_id[t]["status"] == "已验证" for t in group_ids)
        delivery.append(dict(name=group["name"], value=str(done) + " / " + str(len(group_ids))))
    notes.update(scope=config["scope"], current_status=by_id[notes["current_task_id"]]["status"], delivery=delivery)
    batches, warnings = {}, []
    if not isinstance(notes.get("batches"), list):
        raise ValueError("验收批次须为数组")
    for batch in notes["batches"]:
        if not isinstance(batch, dict):
            raise ValueError("验收批次须为对象")
        identity = batch.get("id", "")
        if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_-]*", identity) or identity in batches:
            raise ValueError("证据批次编号重复或非法")
        for key in ("title", "detail", "repository", "report", "document"):
            if not isinstance(batch.get(key), str) or not batch[key]:
                raise ValueError(identity + " 缺少字段：" + key)
        if batch["document"] not in documents:
            raise ValueError(identity + " 文档入口未配置")
        covered = task_ids(batch.get("task_ids", []), set(by_id), "批次任务")
        task_ids(batch.get("verified_task_ids", []), set(covered), "完整验收裁定")
        path = within(repositories[batch["repository"]], batch["report"])
        batch.update(report_exists=path.is_file(), passed=False, failed_stages=[], check_count=0)
        if path.is_file():
            raw = read_json(path)
            batch["report_sha256"] = digest(path)
            if batch.get("sha256") is not None and batch["sha256"] != batch["report_sha256"]:
                raise ValueError(identity + " 原始报告摘要已改变")
            batch["passed"] = raw.get("passed", raw.get("success")) is True
            batch["report_updated_at"] = datetime.fromtimestamp(path.stat().st_mtime, timezone.utc).isoformat()
            stages = raw.get("stages", [])
            if not isinstance(stages, list) or any(not isinstance(stage, dict) for stage in stages):
                raise ValueError(identity + " 报告stages须为对象数组")
            for stage in stages:
                code = stage.get("exit_code", stage.get("code"))
                if stage.get("timed_out") or (code is not None and code != stage.get("expected_code", 0)):
                    batch["failed_stages"].append(stage.get("name", "未命名阶段"))
            checks = raw.get("checks", [])
            batch["check_count"] = checks if type(checks) is int else len(checks) if isinstance(checks, list) else 0
        else:
            warnings.append(identity + " 报告缺失")
        batches[identity] = batch
    for task in tasks:
        if any(identity not in batches for identity in task["evidence_ids"]):
            raise ValueError(task["id"] + " 引用了不存在的证据编号")
        if task["status"] == "已验证" and not any(batches[e]["passed"] and batches[e].get("sha256")
                and task["id"] in batches[e].get("verified_task_ids", []) for e in task["evidence_ids"]):
            raise ValueError(task["id"] + " 缺少成功、摘要固定且覆盖完整退出条件的证据裁定")
    receipts = re.findall(r"^- \[x\] (C\d+) ", text, re.M)
    optional = re.findall(r"^- \[ \] (O\d+) ", text, re.M)
    if len(set(receipts)) != len(receipts) or len(set(optional)) != len(optional):
        raise ValueError("限定成果或可选增强编号重复")
    current = re.search(r"^## 当前[：:]([^\n]+)\n(.*?)(?=^## |\Z)", text, re.M | re.S)
    now = datetime.now(timezone.utc).isoformat()
    return dict(schema_version=1, project_name=config["project_name"], phases=config["phases"],
                all_states=list(STATES), total=len(tasks), states=dict(Counter(t["status"] for t in tasks)),
                tasks=tasks, remaining_person_days=[sum(t["remaining_person_days"][i] for t in tasks) for i in (0, 1)],
                effort_is_uncalibrated=True, completed_scope_receipts=len(receipts), optional_enhancements=len(optional),
                scope_revision=scope["revision"], removed_tasks=removed, generated_at=now, notes=notes,
                source_updated_at=datetime.fromtimestamp(source.stat().st_mtime, timezone.utc).isoformat(),
                current_title=current[1].strip() if current else "以总任务清单为准",
                current_text=current[2].strip() if current else "", warnings=warnings,
                documents=[dict(id=key, name=config["documents"][key]["name"], available=path.is_file()) for key, path in documents.items()])


def sync_plan(root):
    root = Path(root).resolve()
    tasks = {t["id"]: t for t in parse_tasks(read_text(within(root, "docs/TASKS.md")))}
    path = within(root, "docs/EXECUTION-PLAN.md")
    text = read_text(path)
    parts = sections(text)
    if len(parts) != len(tasks) or {p[0] for p in parts} != set(tasks):
        raise ValueError("先按真实范围补齐计划章节；同步不会删除或创造计划")
    def replace(match):
        identity = match[1]
        body = match[3]
        if len(re.findall(r"^状态：[^\n]+$", body, re.M)) != 1:
            raise ValueError(identity + " 计划须有唯一状态镜像")
        body = re.sub(r"^状态：[^\n]+$", "状态：" + tasks[identity]["status"], body, flags=re.M)
        return "### " + identity + " " + tasks[identity]["title"] + body
    updated = re.sub(r"^### (T\d{2}-\d{2}) ([^\n]+)(.*?)(?=^### T\d{2}-\d{2} |\Z)", replace, text, flags=re.M | re.S)
    if updated != text:
        temporary = path.with_name(path.name + "." + uuid.uuid4().hex + ".tmp")
        try:
            temporary.write_text(updated, encoding="utf-8")
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)


def checkpoint(root, note):
    root = Path(root).resolve()
    if not note.strip():
        raise ValueError("保存点必须说明实际结果、未完成内容及下一动作")
    state = load_state(root)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8]
    destination = within(root, "docs/progress/checkpoints/" + stamp)
    destination.mkdir(parents=True, exist_ok=False)
    manifest = dict(schema_version=1, created_at=state["generated_at"], note=note, source_backup=False,
                    files={}, evidence=[])
    for name in TRACKING_FILES:
        original = within(root, name)
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(original, target)
        manifest["files"][name] = digest(target)
    for batch in state["notes"]["batches"]:
        manifest["evidence"].append({k: batch.get(k) for k in ("id", "repository", "report", "report_sha256", "report_exists", "passed")})
    (destination / "status.json").write_text(json.dumps(state, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    manifest["files"]["status.json"] = digest(destination / "status.json")
    (destination / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return destination


def export(root, output):
    root = Path(root).resolve()
    state = load_state(root)
    _, _, documents = settings(root)
    state["embedded_documents"] = {key: read_text(path) for key, path in documents.items() if path.is_file()}
    payload = json.dumps(state, ensure_ascii=False).replace("<", "\\u003c").replace("\u2028", "\\u2028").replace("\u2029", "\\u2029")
    if len(payload.encode("utf-8")) > 16 * 1024 * 1024:
        raise ValueError("离线导出超过16MiB")
    html = read_text(within(root, "docs/progress/PROJECT_TODO.html"))
    if html.count(DATA_MARKER) != 1:
        raise ValueError("页面缺少唯一离线数据入口")
    destination = within(root, output)
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open("x", encoding="utf-8") as writer:
        writer.write(html.replace(DATA_MARKER, DATA_MARKER.replace(">null<", ">" + payload + "<")))
    return destination


def make_server(root, port=None, strict_port=False):
    root = Path(root).resolve()
    config, _, _ = settings(root)
    load_state(root)

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            self.respond()

        def do_HEAD(self):
            self.respond(head=True)

        def respond(self, head=False):
            route = unquote(urlsplit(self.path).path)
            try:
                if route in ("/", "/PROJECT_TODO.html"):
                    body = read_text(within(root, "docs/progress/PROJECT_TODO.html")).encode("utf-8")
                    kind = "text/html; charset=utf-8"
                elif route == "/api/status":
                    body = json.dumps(load_state(root), ensure_ascii=False).encode("utf-8")
                    kind = "application/json; charset=utf-8"
                elif route.startswith("/documents/"):
                    _, _, live_documents = settings(root)
                    if route[11:] not in live_documents:
                        self.send_error(404)
                        return
                    body = read_text(live_documents[route[11:]]).encode("utf-8")
                    kind = "text/plain; charset=utf-8"
                else:
                    self.send_error(404)
                    return
            except (OSError, ValueError, KeyError, TypeError) as error:
                if isinstance(error, FileNotFoundError) and route != "/api/status":
                    self.send_error(404)
                    return
                body = json.dumps({"error": str(error)}, ensure_ascii=False).encode("utf-8")
                self.send_response(503)
                kind = "application/json; charset=utf-8"
            else:
                self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Content-Security-Policy", "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'")
            self.end_headers()
            if not head:
                self.wfile.write(body)

    first = config["port"] if port is None else port
    if type(first) is not int or not 0 <= first <= 65535:
        raise ValueError("端口不合法")
    for candidate in range(first, min(first + (1 if strict_port or first == 0 else 20), 65536)):
        try:
            return ThreadingHTTPServer(("127.0.0.1", candidate), Handler)
        except OSError as error:
            import errno
            if error.errno != errno.EADDRINUSE:
                raise
    raise ValueError("所选端口范围已占用，请指定空闲端口")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check")
    commands.add_parser("sync-plan")
    serve = commands.add_parser("serve")
    serve.add_argument("--port", type=int)
    serve.add_argument("--strict-port", action="store_true")
    saved = commands.add_parser("checkpoint")
    saved.add_argument("--note", required=True)
    offline = commands.add_parser("export")
    offline.add_argument("--output", required=True)
    args = parser.parse_args()
    try:
        if args.command == "serve":
            with make_server(args.root, args.port, args.strict_port) as server:
                print(json.dumps(dict(url="http://127.0.0.1:" + str(server.server_port) + "/PROJECT_TODO.html"), ensure_ascii=False), flush=True)
                try:
                    server.serve_forever()
                except KeyboardInterrupt:
                    pass
        elif args.command == "checkpoint":
            print(checkpoint(args.root, args.note))
        elif args.command == "export":
            print(export(args.root, args.output))
        else:
            if args.command == "sync-plan":
                sync_plan(args.root)
            state = load_state(args.root)
            print(json.dumps({key: state[key] for key in ("project_name", "total", "states", "remaining_person_days", "warnings")}, ensure_ascii=False))
            if state["warnings"]:
                return 1
        return 0
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(json.dumps({"error": str(error)}, ensure_ascii=False), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
