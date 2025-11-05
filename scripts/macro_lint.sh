#!/usr/bin/env bash
# 源码启发式检查：不展开宏、不解析Rust token，不构成完整中立性证明。
# 排除整行 // 注释；允许规范层 parrot_api 与调用方 __parrot_engine 别名。
# 读取/工具失败必须非零退出，不能被 grep 的“无匹配”分支吞掉。
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
from pathlib import Path
import re
import sys

root = Path("crates/parrot-api-derive/src")
files = sorted(root.rglob("*.rs"))
if not files:
    sys.exit(f"FAIL: no Rust sources found in {root}")
pattern = re.compile(r"\bparrot\s*::\s*(?:actix|thread)\b")
violations = []
for path in files:
    source = path.read_text(encoding="utf-8")
    # Retain newlines so reported source lines remain accurate.
    source = "\n".join(
        "" if line.lstrip().startswith("//") else line
        for line in source.split("\n")
    )
    for match in pattern.finditer(source):
        line = source.count("\n", 0, match.start()) + 1
        violations.append(f"{path}:{line}: {match.group()!r}")
if violations:
    print("FAIL: literal engine paths in derive source:")
    print("\n".join(violations))
    sys.exit(1)
print("OK: derive 源码未发现非整行注释中的字面引擎路径（未做宏展开）")
PY
