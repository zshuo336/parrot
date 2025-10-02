#!/bin/zsh
# Akka 对等基准：编译 + 运行（与 Rust 版相同的 8 线程驱动口径）
set -e
cd "$(dirname "$0")"
CP="lib/akka-actor-typed_2.13-2.6.20.jar:lib/akka-actor_2.13-2.6.20.jar:lib/scala-library-2.13.10.jar:lib/config-1.4.2.jar:lib/slf4j-api-1.7.36.jar:out"
mkdir -p out
echo "[compile]"
javac -cp "$CP" -d out src/AkkaBench.java
echo "[run]"
java -cp "$CP" \
  -Xms1g -Xmx1g -XX:+UseZGC \
  -Dakka.actor.default-dispatcher.fork-join-executor.parallelism-min=15 \
  -Dakka.actor.default-dispatcher.fork-join-executor.parallelism-max=15 \
  -Dakka.actor.default-dispatcher.fork-join-executor.parallelism-factor=1.0 \
  AkkaBench 2>&1 | grep -vE "^\[.*INFO|SLF4J"
