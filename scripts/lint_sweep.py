#!/usr/bin/env python3
"""M6 收尾的 clippy 零散项清理（安全模式，逐项显式列出）。

原则：只做「机械可验证」的变换；任何需要判断的一律不动。
"""
import re, os, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

def files_under(roots):
    for root in roots:
        for dirpath, _, fs in os.walk(os.path.join(ROOT, root)):
            for f in fs:
                if f.endswith('.rs'):
                    yield os.path.join(dirpath, f)

def edit(path, fn):
    s = open(path).read()
    s2 = fn(s)
    if s2 != s:
        open(path, 'w').write(s2)
        return True
    return False

total = 0

# ---- 1) redundant pattern matching: `let Ok(_) =` → `.is_ok()` style contexts
# 仅处理 while let Ok(_) = x.try_recv() 与 while let Some(_) = x.try_pop()
def fix_redundant_pat(s):
    s = re.sub(r"while let Ok\(_\) = (.+?)\.try_recv\(\)",
               r"while \1.try_recv().is_ok()", s)
    s = re.sub(r"while let Some\(_\) = (.+?)\.try_pop\(\)",
               r"while \1.try_pop().is_some()", s)
    # let Some(_) = expr { → if expr.is_some() {
    s = re.sub(r"let Some\(_\) = ([\w.]+(?:\(\))?) \{", r"\1.is_some() {", s)
    return s

# ---- 2) assert_eq!(x, true/false) → assert!(x) / assert!(!x)
def fix_bool_assert(s):
    s = re.sub(r"assert_eq!\(([^,]+),\s*true\);", r"assert!(\1);", s)
    s = re.sub(r"assert_eq!\(([^,]+),\s*false\);", r"assert!(!\1);", s)
    return s

# ---- 3) length comparison to zero: len() == 0 → is_empty()
def fix_len_zero(s):
    s = re.sub(r"\.len\(\) == 0", ".is_empty()", s)
    s = re.sub(r"\.len\(\) > 0", r"!\.is_empty()", s)
    return s

# ---- 4) useless vec!: vec![x] 单元素 → Vec::from / 直接改
#（跳过——需人工判断语义；统计里出现的两处手处理）

# ---- 5) unneeded unit expression: match arm 的裸 `()` 行
def fix_unit_expr(s):
    return re.sub(r"\n\s+\(\)\n\s+([A-Za-z_])", r"\n\1", s)

for path in files_under(['parrot/src', 'parrot/tests', 'parrot/examples',
                         'parrot-api/src', 'parrot-api/tests',
                         'parrot-api-derive/src', 'parrot-api-derive-tests/tests']):
    changed = edit(path, lambda s: fix_bool_assert(fix_redundant_pat(s)))
    if changed:
        total += 1
        print("pat/bool:", os.path.relpath(path, ROOT))

print("files touched:", total)
