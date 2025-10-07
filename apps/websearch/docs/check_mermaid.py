#!/usr/bin/env python3
"""websearch 文档 mermaid 块校验（结构级——无需 node 环境）。

用法：python3 apps/websearch/docs/check_mermaid.py [file.md ...]
缺省校验 docs/ 全部 md。GitHub/IDE 渲染前的提交门禁。
"""
import re
import sys
import pathlib

VALID = {
    "flowchart", "graph", "sequenceDiagram", "classDiagram", "stateDiagram",
    "stateDiagram-v2", "erDiagram", "gantt", "pie", "journey", "mindmap",
    "timeline", "gitGraph", "quadrantChart", "xychart-beta", "sankey-beta",
}


def check_block(text: str, src: str, idx: int) -> list:
    errs = []
    lines = text.strip().splitlines()
    if not lines:
        return [f"{src}#{idx}: 空块"]
    head = lines[0].strip()
    kind = head.split()[0] if head.split() else ""
    if kind not in VALID:
        return [f"{src}#{idx}: 未知图类型 {kind!r}（首行：{head[:50]}）"]
    if len(lines) < 2:
        return [f"{src}#{idx}: {kind} 只有声明行（空图）"]
    body = "\n".join(lines[1:])
    if kind in ("flowchart", "graph"):
        depth = 0
        for ln, line in enumerate(body.splitlines(), 2):
            depth += line.count("(") - line.count(")")
            if depth < 0:
                errs.append(f"{src}#{idx} L{ln}: 圆括号负深度（多余的 ')'）")
                depth = 0
        if depth != 0:
            errs.append(f"{src}#{idx}: 圆括号未配对（净深度 {depth}）")
        sg = len(re.findall(r"^\s*subgraph\b", body, re.M))
        en = len(re.findall(r"^\s*end\b", body, re.M))
        if sg != en:
            errs.append(f"{src}#{idx}: subgraph({sg}) / end({en}) 不配对")
    if kind == "sequenceDiagram":
        for ln, line in enumerate(body.splitlines(), 2):
            s = line.strip()
            if not s or s.startswith((
                "%%", "note ", "Note ", "autonumber", "activate", "deactivate",
                "loop", "alt", "else", "end", "rect", "par", "and", "critical",
                "option", "break",
            )):
                continue
            if re.match(r"^(participant|actor)\s", s):
                continue
            if "-->>" in s or "->>" in s or "->" in s or "-)" in s:
                continue
            errs.append(f"{src}#{idx} L{ln}: sequenceDiagram 无法识别的行：{s[:60]}")
    return errs


def main(argv):
    here = pathlib.Path(__file__).parent
    paths = [pathlib.Path(a) for a in argv[1:]] or sorted(here.glob("*.md"))
    total, all_errs = 0, []
    for p in paths:
        text = p.read_text(encoding="utf-8")
        for m in re.finditer(r"```mermaid\n(.*?)```", text, re.S):
            total += 1
            all_errs += check_block(m.group(1), p.name, total)
    print(f"检查 {len(paths)} 文件 · {total} 个 mermaid 块")
    for e in all_errs:
        print("  ✗", e)
    if all_errs:
        sys.exit(1)
    print("  ✓ 全部通过")


if __name__ == "__main__":
    main(sys.argv)
