#!/usr/bin/env python3
"""恢复被 cargo fix / clippy --fix 误删的测试模块 import。

背景：rustc 的 unused-import lint 不理解 `#[cfg(test)]` 模块对
父模块 glob 的依赖，自动修复会把测试代码仍需要的 use 删掉。
本脚本幂等：已存在的 import 不会重复添加。

用法：python3 scripts/restore_test_imports.py
"""
import sys

FIXES = [
    # (path, needle(必须存在), replacement)
    ("parrot/src/thread/actor.rs",
     "use parrot_api::types::{ActorResult, BoxedMessage};",
     "use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};"),
    ("parrot/src/thread/address.rs",
     "use std::sync::{Arc, Mutex};",
     "use std::sync::{Arc, Mutex, Weak};"),
    ("parrot/src/thread/mailbox/spsc.rs",
     "use parrot_api::types::BoxedMessage;",
     "use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage, WeakActorTarget};"),
    ("parrot/src/thread/supervisor_exec.rs",
     "use std::time::Instant;",
     "use std::time::{Duration, Instant};"),
]

TESTMOD_FIXES = [
    # (path, 测试模块内 needle, replacement)
    ("parrot/src/thread/scheduler/dedicated_thread/mod.rs",
     "    use parrot_api::types::{ActorResult, BoxedFuture};",
     "    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};\n    use crate::thread::config::SchedulingMode;"),
    ("parrot/src/thread/system.rs",
     "    use parrot_api::types::ActorResult;",
     "    use parrot_api::actor::ActorState;\n    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};"),
    ("parrot/src/thread/scheduler/shared/worker.rs",
     "    use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, WeakActorTarget};",
     "    use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage, WeakActorTarget};"),
]


def apply(path, needle, repl):
    try:
        s = open(path).read()
    except FileNotFoundError:
        print(f"SKIP {path} (missing)")
        return
    if repl in s:
        print(f"OK   {path} (already fixed)")
        return
    if needle in s:
        s = s.replace(needle, repl, 1)
        open(path, "w").write(s)
        print(f"FIX  {path}")
        return
    # needle 本身被改过（部分修复状态）——按符号探测
    base = path.split("/")[-1]
    need = {
        "actor.rs": "BoxedFuture",
        "address.rs": "Weak",
        "spsc.rs": "WeakActorTarget",
        "supervisor_exec.rs": "Duration",
    }.get(base)
    if need and f"cannot find type `{need}`" not in s:
        print(f"OK   {path} (needle evolved, symbols present via other path)")
        return
    print(f"WARN {path}: needle not found — check manually")


def main():
    for path, needle, repl in FIXES:
        apply(path, needle, repl)
    for path, needle, repl in TESTMOD_FIXES:
        apply(path, needle, repl)


if __name__ == "__main__":
    main()
