#!/usr/bin/env python3
"""Migrate the repository's K10-K28 delivery plan into the progress tracker."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "docs/review/independent-2026-10-08/delivery-20261120/remaining.json"
PROGRESS = ROOT / "docs/progress"
EVIDENCE = PROGRESS / "evidence"


def task_id(package: str, index: int) -> str:
    return f"T{package[1:]}-{index:02d}"


def package_tasks(data: dict) -> list[dict]:
    packages = {item["id"]: item for item in data["tasks"]}
    result = []
    for package in data["tasks"]:
        package_id = package["id"]
        slices = package["slices"]
        package_task_ids = [task_id(package_id, i) for i in range(1, len(slices) + 1)]
        dependency_ids = []
        for dependency in package["final_dependencies"]:
            dependency_ids.append(task_id(dependency, len(packages[dependency]["slices"])))
        for index, slice_ in enumerate(slices, 1):
            identity = task_id(package_id, index)
            slice_id = slice_["id"]
            deliverable = slice_["deliverable"]
            if package["status"] == "completed":
                status = "已验证"
                effort = [0, 0]
                evidence = ["k10-acceptance-20261120"]
            elif package["status"] == "in_progress" and index == 1:
                status = "进行中"
                effort = slice_["engineering_days"]
                evidence = ["factory-admission-final3-local-20261120", "prepared-admission-probe-20261120"]
            else:
                status = "未开始"
                effort = slice_["engineering_days"]
                evidence = []
            dependencies = dependency_ids if index == 1 else [package_task_ids[index - 2]]
            result.append(
                {
                    "id": identity,
                    "package": package_id,
                    "slice": slice_id,
                    "title": f"{slice_id} {deliverable}",
                    "status": status,
                    "effort": effort,
                    "dependencies": dependencies,
                    "package_existing": package["existing"],
                    "deliverable": deliverable,
                    "acceptance": package["acceptance"],
                    "evidence": evidence,
                    "package_acceptance": package["acceptance"],
                }
            )
    return result


def write_tasks(data: dict, tasks: list[dict]) -> None:
    lines = [
        "# Parrot 项目总任务清单",
        "",
        "这是项目进度系统的唯一任务状态来源。任务以 K10-K28 的 55 个可验收切片登记；历史评审与专项报告保留在 `docs/review/independent-2026-10-08/`，不能用报告数量替代退出条件。",
        "",
        "## 当前：K11 运行时剩余契约",
        "",
        "K10 范围与历史证据对账已完成。K11 正在处理工厂/Prepared 饱和清理、取消、生命周期矩阵和双平台整合；当前批次的本机核心回归已通过，Linux Release 仍在执行，Prepared 入口已有旧实现反例但尚未完成生产修复。K12-K28 保持必做范围，4 个可选包只作为增强项。",
        "",
        "## 必需任务",
        "",
    ]
    for task in tasks:
        evidence = ",".join(task["evidence"]) or "无"
        deps = ",".join(task["dependencies"]) or "无"
        implemented = (
            task["package_existing"]
            + (" 当前切片已有本机/专项证据，但完整整包验收仍未关闭。" if task["status"] == "进行中" else "")
        )
        remaining = "无；本切片退出条件已由固定报告逐条裁定。" if task["status"] == "已验证" else task["deliverable"]
        lines.extend(
            [
                f"### {task['id']} {task['title']}",
                f"状态：{task['status']}",
                f"剩余人日：{task['effort'][0]}-{task['effort'][1]}",
                f"依赖：{deps}",
                f"已做：{implemented}",
                f"待做：{remaining}",
                f"验收：{task['acceptance']}",
                f"证据：{evidence}",
                "",
            ]
        )
    lines.extend(
        [
            "## 限定成果收据",
            "",
            "- [x] C01 K10 范围、历史证据、feature 与示例清单完成对账；完整实现验收仍由 K11-K28 负责。",
            "- [x] C02 FACTORYADMISSION01 本机核心回归与确定性 Actix 取消竞态修复证据已登记；Linux final3 尚未终态，不能关闭 K11。",
            "- [x] C03 Prepared 饱和入口旧实现反例已保留；该收据只证明缺陷存在，不代表修复完成。",
            "",
            "## 可选增强",
            "",
            "- [ ] O01 X01 300000/s 性能目标；A5/M4 原有门槛不降低。",
            "- [ ] O02 百万驻留规模实验；不替代至少 72 小时长稳验收。",
            "- [ ] O03 物理断电/真实外部服务故障实验；无设备或服务时保留未认证。",
            "- [ ] O04 额外跨语言与平台组合；不抵扣 K19/K26 的必做矩阵。",
            "",
        ]
    )
    (ROOT / "docs/TASKS.md").write_text("\n".join(lines), encoding="utf-8")


def write_plan(tasks: list[dict]) -> None:
    lines = [
        "# Parrot 完整执行计划",
        "",
        "本计划由 TASKS.md 同步状态；每个切片保留实现、验证和退出边界。剩余人日是低置信度工程量区间，不是日历承诺。",
        "",
    ]
    for task in tasks:
        deps = ",".join(task["dependencies"]) or "无"
        evidence = ",".join(task["evidence"]) or "无"
        lines.extend(
            [
                f"### {task['id']} {task['title']}",
                f"状态：{task['status']}",
                f"实现：{task['package_existing']} 目标是：{task['deliverable']}",
                f"验证：依赖 {deps}；使用真实代码、测试、目标环境和原始报告，证据入口 {evidence}。",
                f"退出：{task['acceptance']} 本切片只有在剩余工作为 0、依赖已验证且报告固定后才能标为已验证。",
                "",
            ]
        )
    (ROOT / "docs/EXECUTION-PLAN.md").write_text("\n".join(lines), encoding="utf-8")


def write_json(data: dict, tasks: list[dict]) -> None:
    ids = [task["id"] for task in tasks]
    phases = {f"T{number:02d}": title for number, title in [(10, "范围和证据"), (11, "运行时契约"), (12, "消息会话"), (13, "安全和资源"), (14, "业务持久性"), (15, "共识运维"), (16, "自动接管"), (17, "编排部署"), (18, "Wasm 和原生"), (19, "跨语言"), (20, "观测回放"), (21, "测试模型"), (22, "性能优化"), (23, "规模长稳"), (24, "示例演示"), (25, "文档兼容"), (26, "平台认证"), (27, "发布工程"), (28, "综合结项")]}
    config = {
        "schema_version": 1,
        "project_name": "Parrot 分布式 Actor 运行时",
        "scope": "Parrot 代码、文档、示例、跨语言互操作、测试、部署、性能和最终交付的真实实现与验收",
        "port": 8782,
        "repositories": {"main": "."},
        "phases": phases,
        "documents": {
            "tasks": {"name": "总任务清单", "repository": "main", "path": "docs/TASKS.md"},
            "plan": {"name": "完整执行计划", "repository": "main", "path": "docs/EXECUTION-PLAN.md"},
            "index": {"name": "跨文档入口", "repository": "main", "path": "docs/TODO-MASTER.md"},
            "scope": {"name": "范围与移除记录", "repository": "main", "path": "docs/progress/scope.json"},
            "review": {"name": "独立综合评审与历史证据", "repository": "main", "path": "docs/review/independent-2026-10-08/PROJECT_TODO.md"},
            "delivery": {"name": "当前剩余交付计划", "repository": "main", "path": "docs/review/independent-2026-10-08/delivery-20261120/REMAINING.md"},
            "factory-admission": {"name": "工厂生命周期专项", "repository": "main", "path": "docs/review/independent-2026-10-08/factory-lifecycle-admission-20261120/README.md"},
            "prepared-admission": {"name": "Prepared 清理入口专项", "repository": "main", "path": "docs/review/independent-2026-10-08/prepared-lifecycle-admission-20261120/README.md"},
            "readme": {"name": "项目说明", "repository": "main", "path": "README.md"},
            "testing": {"name": "核心测试说明", "repository": "main", "path": "crates/parrot/tests/README.md"},
        },
    }
    (PROGRESS / "config.json").write_text(json.dumps(config, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    (PROGRESS / "scope.json").write_text(json.dumps({"schema_version": 1, "revision": "parrot-k10-k28-2026-11-20", "active_task_ids": ids, "removed_tasks": []}, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    (PROGRESS / "current.json").write_text(json.dumps({
        "schema_version": 1,
        "updated": "2026-11-20",
        "scope_change": "迁移现有 K10-K28 计划为 55 个真实可验收切片；未移除任何必做范围，4 个可选增强单独保留。",
        "headline": "K11 运行时剩余契约进行中",
        "current_task_id": "T11-01",
        "current_detail": "FACTORYADMISSION01 已完成本机核心回归并修复确定性 Actix cohort 取消竞态；Prepared 饱和入口已复现旧实现问题，正在设计保留生命周期所有者的生产修复。Linux final3 仍在执行。",
        "boundary": "K10 仅关闭范围与证据对账；K11-K28 的实现、跨语言、部署、性能、长稳和最终整包验收仍未完成。局部测试与历史批次不继承为当前工作树的完整认证。",
        "queue": ids,
        "delivery_groups": [
            {"name": "K10 范围基线", "task_ids": ["T10-01"]},
            {"name": "K11 运行时契约", "task_ids": [task["id"] for task in tasks if task["package"] == "K11"]},
            {"name": "K12-K20 核心能力", "task_ids": [task["id"] for task in tasks if 12 <= int(task["package"][1:]) <= 20]},
            {"name": "K21-K28 质量与交付", "task_ids": [task["id"] for task in tasks if 21 <= int(task["package"][1:]) <= 28]},
        ],
        "batches": [],
        "limits": [
            "当前任务计数按 55 个切片显示，不将任务数量当作工程量完成百分比。",
            "K11 final3 Linux 尚未终态；本机通过不替代 Linux、Release、跨平台和整包验收。",
            "Prepared 饱和旧实现反例保留，修复复验必须建立新冻结批次，不能改写旧失败。",
            "远程 Linux 访问已获用户持续授权；上传包仍排除 .git、凭据和本机配置目录。",
            "剩余人日区间为低置信度规划，不是日历完成日期或模型运行时间。",
        ],
    }, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def write_evidence(tasks: list[dict]) -> None:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    reports = {
        "k10-acceptance-20261120": {
            "passed": True,
            "candidate": "K10 acceptance records through 2026-11-20",
            "checks": 6,
            "stages": [{"name": "scope-mapping"}, {"name": "review-cross-index"}, {"name": "feature-matrix"}, {"name": "example-inventory"}, {"name": "estimate-reconciliation"}, {"name": "document-audit"}],
            "verified_task_ids": ["T10-01"],
            "limitations": ["This batch closes range and evidence accounting only; it does not verify K11-K28 implementation."],
        },
        "factory-admission-final3-local-20261120": {
            "passed": True,
            "candidate": "2703-input local final3 source",
            "checks": 4,
            "stages": [{"name": "core-tests", "exit_code": 0}, {"name": "doctests", "exit_code": 0}, {"name": "clippy", "exit_code": 0}, {"name": "fmt", "exit_code": 0}],
            "verified_task_ids": [],
            "limitations": ["Local core evidence only; Linux final3, Prepared production repair and full K11 acceptance remain open."],
        },
        "prepared-admission-probe-20261120": {
            "passed": False,
            "candidate": "current worktree Prepared saturation probe",
            "checks": 2,
            "stages": [{"name": "thread-prepared-saturation", "exit_code": 101}, {"name": "actix-prepared-saturation", "exit_code": 101}],
            "verified_task_ids": [],
            "limitations": ["This is a preserved old-implementation/current-probe failure demonstrating the defect; it is not a repair result."],
        },
    }
    for identity, report in reports.items():
        path = EVIDENCE / f"{identity}.json"
        path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    batches = []
    for identity, report in reports.items():
        path = EVIDENCE / f"{identity}.json"
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        batches.append({
            "id": identity,
            "title": report["candidate"],
            "detail": "固定报告、候选范围、执行阶段和已裁定边界见原始 JSON。",
            "repository": "main",
            "report": f"docs/progress/evidence/{identity}.json",
            "document": "tasks",
            "task_ids": ["T10-01"] if identity == "k10-acceptance-20261120" else ["T11-01"],
            "verified_task_ids": report["verified_task_ids"],
            "sha256": digest,
        })
    current = json.loads((PROGRESS / "current.json").read_text(encoding="utf-8"))
    current["batches"] = batches
    (PROGRESS / "current.json").write_text(json.dumps(current, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def main() -> None:
    data = json.loads(SOURCE.read_text(encoding="utf-8"))
    tasks = package_tasks(data)
    if len(tasks) != 55:
        raise SystemExit(f"expected 55 tasks, got {len(tasks)}")
    write_tasks(data, tasks)
    write_plan(tasks)
    write_json(data, tasks)
    write_evidence(tasks)
    print(json.dumps({"tasks": len(tasks), "verified": 1, "in_progress": 1, "pending": 53}, ensure_ascii=False))


if __name__ == "__main__":
    main()
