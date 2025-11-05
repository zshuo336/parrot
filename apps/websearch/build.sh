#!/usr/bin/env bash
# ============================================================================
#  build.sh —— websearch 全组件构建（五运行时制品）
#
#  Rust   target/{release,debug}/websearch     漫爬+编排+Web 服务
#  JVM    jvm/target/websearch-jvm-1.0.0.jar   检索组件（jieba-analysis）
#  Erlang erlang/frontier.erl                  源码即制品（网关热加载）
#  Ray    python/tokenizer.py                  源码即制品（jieba 分词）
#  TS     web/                                 静态页（Rust 直接内嵌服务）
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")"

echo "==> [1/4] Rust 应用"
if [ "$#" -eq 0 ]; then set -- --release; fi
( cd ../.. && cargo build --locked -p websearch "$@" )
echo "    ✓ target 里 websearch"

echo "==> [2/4] JVM 检索组件（jieba-analysis + BM25）"
bash jvm/build.sh
echo "    ✓ jvm/target/websearch-jvm-1.0.0.jar"

echo "==> [3/4] Erlang frontier（beam 预编译——网关热加载制品）"
(cd erlang && erlc frontier.erl)
echo "    ✓ erlang/frontier.beam"

echo "==> [4/4] 依赖自检"
python3 -c "import jieba" || { echo "缺 jieba（pip3 install jieba）" >&2; exit 1; }
echo "    ✓ python jieba"
python3 -c "import ray" || { echo "缺 ray（pip3 install ray）" >&2; exit 1; }
echo "    ✓ python ray"
echo "✓ websearch 构建完成——run.sh <seed-url> 启动"
