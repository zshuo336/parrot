#!/bin/zsh
# Erlang 对等基准：编译 + 运行（escript 入口规避 erl -eval boot watchdog）
set -e
cd "$(dirname "$0")"
mkdir -p out
echo "[compile]"
erlc -W0 -o out parrot_bench.erl
echo "[run]"
escript run_bench.escript
