#!/usr/bin/env bash
# parrot-probe 全链验证（单脚本——同 shell 生命周期内起网关+测全部子命令）
set -uo pipefail
REPO=/Users/biluochun/work/error.d/library/parrot
cd "$REPO"
PROBE=./target/debug/parrot-obs

cleanup() { pkill -f 'parrot_gw:main' 2>/dev/null; pkill -f 'ParrotGatewayMain' 2>/dev/null; pkill -f 'ray_gw' 2>/dev/null; sleep 1; }
trap cleanup EXIT
cleanup

# 起三网关（direct 端口形态）
(cd interop/erlang && erl -noinput -noshell -pa . -eval 'parrot_gw:main(["19871"])' > /tmp/pb_erl.out 2>&1) &
ERL=$!
(cd interop/jvm/target && exec java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat interop/jvm/target/cp.txt 2>/dev/null || cat cp.txt)" parrot.protocol.jvm.ParrotGatewayMain 19872 node=jvm-search-1 7200 > /tmp/pb_jvm.out 2>&1) &
JVM=$!
(cd interop/python && exec env PYTHONPATH=. python3 -m parrot_protocol.ray_gw 19873 > /tmp/pb_ray.out 2>&1) &
RAY=$!

for i in $(seq 1 40); do
  ok=0
  grep -q PARROT_ERL_PORT /tmp/pb_erl.out 2>/dev/null && ok=$((ok+1))
  grep -q PARROT_JVM_PORT /tmp/pb_jvm.out 2>/dev/null && ok=$((ok+1))
  grep -q RAY_GW_PORT /tmp/pb_ray.out 2>/dev/null && ok=$((ok+1))
  [ $ok -eq 3 ] && break
  sleep 1
done
[ $ok -eq 3 ] || { echo "网关起失败"; tail -3 /tmp/pb_{erl,jvm,ray}.out; exit 1; }
echo "══ 三网关就绪（erl/jvm/ray）"

NODES="erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872"

echo "══ 1/6 status"
$PROBE status $NODES || exit 1

echo "══ 2/7 ping（admin 通道 RTT 3 轮）"
$PROBE ping $NODES --rounds 3 || exit 1

echo "══ 3/7 ask（业务通道 echo 探针——三方言 Ping→Pong）"
$PROBE ask $NODES --rounds 3 || exit 1

echo "══ 4/7 metrics（全表——ask 后计数应非零）"
$PROBE metrics $NODES || exit 1

echo "══ 5/7 trace + load（8s 压测窗口差分——load 进程内并发 ask）"
$PROBE load $NODES --seconds 6 --conc 4 > /tmp/pb_load.out 2>&1 &
LOAD=$!
sleep 4   # 等 load 握手完成（单连接网关串行 accept——避免竞争）
$PROBE trace $NODES --seconds 8 || exit 1
wait $LOAD 2>/dev/null
echo "── load 结果："; cat /tmp/pb_load.out | tail -2

echo "══ 6/7 watch（4s 两轮采样——kill 硬停非失败）"
$PROBE watch $NODES --interval 2 > /tmp/pb_watch.out 2>&1 &
W=$!
sleep 5; kill $W 2>/dev/null
grep -q 'asks' /tmp/pb_watch.out && echo "  ✓ watch 采样输出在位" || { echo "watch 无输出"; cat /tmp/pb_watch.out; exit 1; }

echo "══ 7/7 web（起 3s 验 /api/metrics 与页面）"
$PROBE web $NODES --web-port 8199 > /tmp/pb_web.out 2>&1 &
WEB=$!
API=""
for i in $(seq 1 15); do
  API=$(curl -s --max-time 3 http://localhost:8199/api/metrics 2>/dev/null)
  [ -n "$API" ] && break
  sleep 1
done
[ -n "$API" ] || { echo "web 起失败"; cat /tmp/pb_web.out; kill $WEB; exit 1; }
echo "$API" | python3 -c "
import json,sys
d=json.load(sys.stdin)
assert len(d)==3, f'节点数 {len(d)}'
for k,v in d.items():
    assert 'runtime' in v, f'{k} 无 runtime: {v}'
    assert v['ts_ms']>0
print('  ✓ /api/metrics 三节点快照：', {k:v['runtime'] for k,v in d.items()})
" || { kill $WEB; exit 1; }
PAGE=$(curl -s --max-time 3 http://localhost:8199/)
echo "$PAGE" | grep -q 'parrot-obs' && echo "  ✓ 页面渲染（title 含 parrot-obs）"
echo "$PAGE" | grep -q 'api/metrics' && echo "  ✓ 页面 JS 拉取端点在位"
kill $WEB 2>/dev/null

echo "══ 全部通过 ✓"
