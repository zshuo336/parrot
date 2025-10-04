#!/usr/bin/env bash
# 宏中立性 lint（M1 防回归门禁）：
# derive 宏的【生成代码模板】不得引用任何引擎符号路径
# （parrot::actix / parrot::thread 等实现层路径）。
# - 只检查生成段：quote! {...} 块内的 token（文档注释 /// 排除）
# - 规范层符号 parrot_api:: 允许
# - __parrot_engine 别名面允许（这正是解耦机制）
set -euo pipefail

cd "$(dirname "$0")/.."

# 剥离注释行后 grep：宏 crate 源码中任何非注释位置的 parrot::actix / parrot::thread
# 都意味着生成模板（或属性解析逻辑）硬编码了引擎路径。
VIOLATIONS=$(grep -rn 'parrot::actix\|parrot::thread' crates/parrot-api-derive/src/ \
  | grep -v '^\s*[^:]*:[0-9]*:\s*///' \
  | grep -v '^\s*[^:]*:[0-9]*:\s*//!' \
  | grep -v '^\s*[^:]*:[0-9]*:\s*//' || true)

if [ -n "$VIOLATIONS" ]; then
  echo "FAIL: derive crate 生成模板/逻辑引用了引擎符号："
  echo "$VIOLATIONS"
  echo ""
  echo "修复方向：生成代码一律经 __parrot_engine::* 别名面；"
  echo "文档示例中的引擎绑定写法允许出现在 /// 注释里。"
  exit 1
fi

echo "OK: derive 宏生成模板零引擎符号引用（文档示例已豁免）"
