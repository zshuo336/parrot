#!/usr/bin/env bash
# 可移植超时包装器（macOS 无 coreutils timeout）。
# 用法: with_timeout.sh <秒数> <命令...>
# 退出码: 124 = 超时；其余透传子命令。
#
# 卡死防护设计（各层均实测验证）：
# 1. 子命令独立进程组（set -m + &）：超时后 bash 内建 kill 对负 PID
#    杀整组——rustc/cargo/测试子进程全灭；2s 后仍存活则 SIGKILL
#    （防"忽略 TERM 的顽固进程"，实测覆盖）；
# 2. 监视器 sub-shell 第一条语句重定向全部 fd 到 /dev/null：
#    其内部 sleep 不继承输出管道 → 主命令退出后管道立即 EOF，
#    下游 grep/tail 不会因监视器残留挂起（第一版曾因此挂死 12 分钟）；
# 3. 主命令结束即 TERM 收割监视器（bash 无 handle 的默认 TERM 立即
#    生效，wait 不会阻塞）；孤儿的 sleep 只持有 /dev/null，无害；
# 4. 残余进程组双保险清扫。
set -u
secs="$1"; shift
if [[ -z "${secs}" || "$#" -eq 0 ]]; then
  echo "usage: $0 <seconds> <command...>" >&2
  exit 2
fi

set -m
"$@" &
child=$!
set +m

(
  # fd 隔离：绝不持有调用方管道
  exec 0</dev/null 1>/dev/null 2>/dev/null
  sleep "$secs"
  kill -TERM -- -"$child" 2>/dev/null
  sleep 2
  kill -KILL -- -"$child" 2>/dev/null
) &
watcher=$!

wait "$child"
rc=$?

# 收割监视器（TERM 即死；孤儿 sleep 无害）
kill -TERM "$watcher" 2>/dev/null
wait "$watcher" 2>/dev/null

# 残余进程组双保险
if kill -0 -- -"$child" 2>/dev/null; then
  kill -TERM -- -"$child" 2>/dev/null
  sleep 1
  kill -KILL -- -"$child" 2>/dev/null
fi

# 143 = 128+SIGTERM；137 = 128+SIGKILL：被超时杀掉
if [[ $rc -eq 143 || $rc -eq 137 ]]; then
  echo "[with_timeout] 命令超时（>${secs}s）被终止" >&2
  exit 124
fi
exit $rc
