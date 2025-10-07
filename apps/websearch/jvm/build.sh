#!/usr/bin/env bash
# ============================================================================
#  build.sh —— websearch jvm 检索组件构建（同 crawler-lab/jvm 模式）
#
#  产物：apps/websearch/jvm/target/websearch-jvm-1.0.0.jar（thin）
#  classpath 含 lib/jieba-analysis-1.0.2.jar（中文分词——child-first 数据源）
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")"

M2="${HOME}/.m2/repository"
V_SCALA=2.13.16
V_AKKA=2.6.20
SCALA_LIB="$M2/org/scala-lang/scala-library/$V_SCALA/scala-library-$V_SCALA.jar"
SCALA_CMP="$M2/org/scala-lang/scala-compiler/$V_SCALA/scala-compiler-$V_SCALA.jar"
SCALA_REF="$M2/org/scala-lang/scala-reflect/$V_SCALA/scala-reflect-$V_SCALA.jar"
AKKA_TYPED="$M2/com/typesafe/akka/akka-actor-typed_2.13/$V_AKKA/akka-actor-typed_2.13-$V_AKKA.jar"
GW_JAR="../../../interop/jvm/target/parrot-protocol-jvm-0.1.0.jar"
JIEBA="lib/jieba-analysis-1.0.2.jar"

for j in "$SCALA_CMP" "$SCALA_LIB" "$SCALA_REF" "$AKKA_TYPED" "$GW_JAR" "$JIEBA"; do
  [ -f "$j" ] || { echo "缺依赖：$j（gw jar 先 make build-jvm；jieba 见 README）"; exit 1; }
done

mkdir -p target/classes
echo "==> scalac 编译 websearch.search.SearchComponent"
java -cp "$SCALA_CMP:$SCALA_LIB:$SCALA_REF" scala.tools.nsc.Main \
  -usejavacp -encoding UTF-8 \
  -classpath "$AKKA_TYPED:$GW_JAR:$JIEBA" \
  -d target/classes \
  src/main/scala/websearch/search/SearchComponent.scala

echo "==> 解包 jieba-analysis 入 classes（fat jar——child-first 加载器自洽）"
JAR_ABS="$PWD/lib/jieba-analysis-1.0.2.jar"
(cd target/classes && unzip -oq "$JAR_ABS" "com/*" "dict.txt" "prob_emit.txt")

echo "==> 打 fat jar"
jar cf target/websearch-jvm-1.0.0.jar -C target/classes .
ls -la target/websearch-jvm-1.0.0.jar
echo "✓ apps/websearch/jvm/target/websearch-jvm-1.0.0.jar（含 jieba+词典）"
