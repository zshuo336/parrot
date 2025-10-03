#!/bin/zsh
# Ray 对等基准（降级版）：15 核 / 10GB 内存 / ≤5 分钟
# 全程 with_timeout 防挂死；场景内所有 ray.get 带超时
# 输出保留原始 stderr（Ray 诊断信息分析时有用），汇总行以 "[ray]" 前缀提取
set -e
cd "$(dirname "$0")"
rm -rf /tmp/ray_bench_tmp
ray stop --force 2>/dev/null || true
sleep 1
exec ../../scripts/with_timeout.sh 400 python3 ray_bench.py 2>&1
