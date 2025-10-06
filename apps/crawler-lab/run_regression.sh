#!/usr/bin/env bash
# ============================================================================
#  G3/M1 行为等价回归判定（DEV_09 §5.6 DoD 命令表：run_regression.sh）
#
#  语义：用 golden 录制同参数重跑 crawler-lab 四运行时全链，比对关键断言
#  行（fetched/pushed_total/terms/postings/搜索 top-5/PASS 行）——迁移前后
#  cid 轨迹等价。
#
#  golden 来源：apps/crawler-lab/golden/pre_migration_output.txt
#  （master @ 0962e08 · pages=60 depth=2 fanout=3 batch=32 · 2026-10-06）
#
#  用法：apps/crawler-lab/run_regression.sh [--keep-output]
#  依赖：erl / java(interop/jvm/target/*.jar) / python3+ray / node —— 缺则失败
#  输出：PASS → exit 0；任何断言漂移 → 差异清单 + exit 1
# ============================================================================
set -uo pipefail
cd "$(dirname "$0")/../.."
ROOT="$PWD"

GOLDEN="apps/crawler-lab/golden/pre_migration_output.txt"
OUT="/tmp/crawler_regression.out"

echo "==> [1/3] 构建 crawler-lab"
cargo build -p crawler-lab -q --release || { echo "构建失败"; exit 1; }

echo "==> [2/3] 四运行时全链（golden 同参数：pages=60 depth=2 fanout=3 batch=32）"
if ! ./deploy/crawler-lab/run-lab.sh --pages 60 --depth 2 --fanout 3 --batch 32 --skip-ts > "$OUT" 2>&1; then
  echo "run-lab 失败——尾部日志："
  tail -15 "$OUT"
  exit 1
fi

echo "==> [3/3] 关键断言比对（golden vs 本次）"
fail=0

check() {  # check <描述> <grep 模式>
  local desc="$1" pat="$2"
  if grep -qE "$pat" "$OUT"; then
    echo "  ✓ $desc"
  else
    echo "  ✗ $desc ——本次输出缺失该断言（漂移）"
    echo "    期待模式：$pat"
    fail=1
  fi
}

check "爬取计数（fetched=60 去重）"          'fetched=60（去重后）'
check "推送总数 pushed_total=99"             'pushed_total=99'
check "ray 索引 terms=33"                    'ray 索引: terms=33'
check "ray 索引 postings=1746"               'postings=1746'
check "jvm 倒排 {\"terms\":33,\"postings\":1746}" '\{"terms":33,"postings":1746'
check "终态 PASS 行"                         'crawler-lab PASS'

# 搜索 top-5 结果对（golden 四组 query 的 doc/score——精确锚定行模式）
check "search [parrot] top-5"          'search \["parrot"\] → \[\{"doc":9,"score":7\},\{"doc":28,"score":7\},\{"doc":16,"score":5\},\{"doc":21,"score":5\},\{"doc":42,"score":5\}\]'
check "search [actor,cluster] top-5"   'search \["actor", "cluster"\] → \[\{"doc":39,"score":10\},\{"doc":1,"score":9\},\{"doc":27,"score":9\},\{"doc":51,"score":9\},\{"doc":7,"score":8\}\]'
check "search [crawler,index,search]"  'search \["crawler", "index", "search"\] → \[\{"doc":13,"score":19\},\{"doc":9,"score":15\},\{"doc":35,"score":14\},\{"doc":6,"score":12\},\{"doc":7,"score":12\}\]'
check "search [raft,swim] top-5"       'search \["raft", "swim"\] → \[\{"doc":28,"score":12\},\{"doc":51,"score":11\},\{"doc":16,"score":9\},\{"doc":20,"score":9\},\{"doc":10,"score":8\}\]'
check "jvm healthz（queries=4）"       'jvm healthz: \{"terms":33,"postings":1746,"queries":4\}'
check "终态计数行 done"                'pushed=   99 fetched=   60 ray_pages=   60 jvm_terms=   1746'

if [ "$fail" -ne 0 ]; then
  echo ""
  echo "回归失败——行为漂移（完整输出：$OUT）"
  exit 1
fi

echo ""
echo "crawler-lab 回归 PASS（迁移前后 cid 轨迹等价——G1/M1 行为等价门禁）"

# --keep-output：保留现场排查
if [[ " $* " != *" --keep-output "* ]]; then
  rm -f "$OUT"
fi
exit 0
