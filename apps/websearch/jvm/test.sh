#!/usr/bin/env bash
# Build current component and run real actor/storage/ranking contracts.
set -euo pipefail
cd "$(dirname "$0")"
bash build.sh
TEST_CLASSES=$(mktemp -d "$PWD/target/contracts.XXXXXX")
trap 'rm -rf "$TEST_CLASSES"' EXIT
SCALA_HOME_JARS="$HOME/.m2/repository/org/scala-lang"
COMPILER="$SCALA_HOME_JARS/scala-compiler/2.13.16/scala-compiler-2.13.16.jar:$SCALA_HOME_JARS/scala-library/2.13.16/scala-library-2.13.16.jar:$SCALA_HOME_JARS/scala-reflect/2.13.16/scala-reflect-2.13.16.jar"
GW_DIR=../../../interop/jvm/target
CP="target/websearch-jvm-1.0.0.jar:$GW_DIR/parrot-protocol-jvm-0.1.0.jar:$(cat "$GW_DIR/cp.txt")"
java -cp "$COMPILER" scala.tools.nsc.Main -usejavacp -encoding UTF-8 \
  -classpath "$CP" -d "$TEST_CLASSES" src/test/scala/websearch/search/*.scala
java -cp "$TEST_CLASSES:$CP" websearch.search.SearchContracts
java -cp "$TEST_CLASSES:$CP" websearch.search.SearchConfigContracts
java -cp "$TEST_CLASSES:$CP" websearch.search.DocumentReplayContracts "$@"
java -cp "$TEST_CLASSES:$CP" websearch.search.CrawlCheckpointContracts
java -cp "$TEST_CLASSES:$CP" websearch.search.SnapshotHistoryContracts
java -cp "$TEST_CLASSES:$CP" websearch.search.SnapshotCompactionContracts
java -cp "$TEST_CLASSES:$CP" websearch.search.StorageContracts
