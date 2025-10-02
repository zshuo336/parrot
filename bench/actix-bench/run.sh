#!/bin/zsh
# 纯 Actix 基准：编译 + 运行（与 akka-bench/run.sh 平级对称）
set -e
cd "$(dirname "$0")"
echo "[build]"
cargo build --release
echo "[run]"
./target/release/actix-bench
