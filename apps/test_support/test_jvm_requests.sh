#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
mvn -q -s interop/jvm/settings-central.xml -f interop/jvm/pom.xml package dependency:build-classpath -Dmdep.outputFile=target/cp.txt -DskipTests
mvn -q -s interop/jvm/settings-central.xml -f apps/crawler-lab/jvm/pom.xml package -DskipTests
bash apps/websearch/jvm/build.sh
classes=$(mktemp -d)
trap 'rm -rf "$classes"' EXIT
scala_jars="$HOME/.m2/repository/org/scala-lang"
compiler="$scala_jars/scala-compiler/2.13.16/scala-compiler-2.13.16.jar:$scala_jars/scala-library/2.13.16/scala-library-2.13.16.jar:$scala_jars/scala-reflect/2.13.16/scala-reflect-2.13.16.jar"
cp="interop/jvm/target/parrot-protocol-jvm-0.1.0.jar:apps/websearch/jvm/target/websearch-jvm-1.0.0.jar:apps/crawler-lab/jvm/target/crawler-lab-jvm-1.0.0.jar:$(cat interop/jvm/target/cp.txt)"
java -cp "$compiler" scala.tools.nsc.Main -usejavacp -encoding UTF-8 -classpath "$cp" -d "$classes" apps/test_support/JvmRequestContracts.scala
java -cp "$classes:$cp" parrot.apps.contracts.JvmRequestContracts
