#!/usr/bin/env bash
# ============================================================================
#  build-app-jvm.sh —— crawler-lab 应用 jvm 组件构建（R1/R4）
#
#  产物：apps/crawler-lab/jvm/target/crawler-lab-jvm-1.0.0.jar
# 依赖：interop/jvm/target/parrot-protocol-jvm-0.1.0.jar（make build-jvm 先行）
#
#  说明：mvn 离线环境（scala-maven-plugin 无法解析）时退化为 scalac 直编
#  （依赖自 ~/.m2 缓存取——与网关 pom 同版本）。产物为 thin jar：akka/scala
#  运行期由网关 classloader 提供（child-first 只取本 jar 的组件类）。
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")"

M2="${HOME}/.m2/repository"
V_SCALA=2.13.16
V_AKKA=2.6.20
SCALA_LIB="$M2/org/scala-lang/scala-library/$V_SCALA/scala-library-$V_SCALA.jar"
SCALA_CMP="$M2/org/scala-lang/scala-compiler/$V_SCALA/scala-compiler-$V_SCALA.jar"
SCALA_REF="$M2/org/scala-lang/scala-reflect/$V_SCALA/scala-reflect-$V_SCALA.jar"
AKKA="$M2/com/typesafe/akka/akka-actor-typed_2.13/$V_AKKA/akka-actor-typed_2.13-$V_AKKA.jar"
GW_JAR="../../../interop/jvm/target/parrot-protocol-jvm-0.1.0.jar"

[ -f "$GW_JAR" ] || { echo "缺网关 jar——先 make build-jvm"; exit 1; }

mkdir -p target/classes
echo "==> scalac 编译 crawler.search.SearchComponent"
java -cp "$SCALA_CMP:$SCALA_LIB:$SCALA_REF" scala.tools.nsc.Main \
  -usejavacp -encoding UTF-8 \
  -classpath "$AKKA:$GW_JAR" \
  -d target/classes \
  src/main/scala/crawler/search/SearchComponent.scala

echo "==> 打 thin jar"
jar cf target/crawler-lab-jvm-1.0.0.jar -C target/classes .
echo "✓ apps/crawler-lab/jvm/target/crawler-lab-jvm-1.0.0.jar"
