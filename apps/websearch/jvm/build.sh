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
AKKA_CLASSIC="$M2/com/typesafe/akka/akka-actor_2.13/$V_AKKA/akka-actor_2.13-$V_AKKA.jar"
GW_JAR="../../../interop/jvm/target/parrot-protocol-jvm-0.1.0.jar"
JIEBA="lib/jieba-analysis-1.0.2.jar"

for j in "$SCALA_CMP" "$SCALA_LIB" "$SCALA_REF" "$AKKA_TYPED" "$AKKA_CLASSIC" "$GW_JAR" "$JIEBA"; do
  [ -f "$j" ] || { echo "缺依赖：$j（gw jar 先 make build-jvm；jieba 见 README）"; exit 1; }
done

mkdir -p target
# Pin both the resolver and the small parser dependency closure. Ship it in the
# component jar: remote gateways must not depend on the builder's Maven cache.
BUILD_DIR=$(mktemp -d "$PWD/target/build.XXXXXX")
trap 'rm -rf "$BUILD_DIR"' EXIT
mkdir "$BUILD_DIR/classes"
mvn -q -f config-dependencies.pom.xml org.apache.maven.plugins:maven-dependency-plugin:3.8.1:copy-dependencies \
  -DincludeScope=runtime -DoutputDirectory="$BUILD_DIR/config-libs"
python3 - "$BUILD_DIR/config-libs" <<'CHECK'
from pathlib import Path
import hashlib, sys
root = Path(sys.argv[1])
expected = dict(line.split()[::-1] for line in Path('config-dependencies.sha256').read_text().splitlines())
actual = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in root.glob('*.jar')}
if actual != expected:
    raise SystemExit('configuration dependency checksum/inventory mismatch')
CHECK
CONFIG_CP=$(python3 -c 'from pathlib import Path; import sys; print(":".join(str(p) for p in sorted(Path(sys.argv[1]).glob("*.jar"))))' "$BUILD_DIR/config-libs")
echo "==> scalac 编译 websearch.search.SearchComponent"
java -cp "$SCALA_CMP:$SCALA_LIB:$SCALA_REF" scala.tools.nsc.Main \
  -usejavacp -encoding UTF-8 \
  -classpath "$AKKA_TYPED:$AKKA_CLASSIC:$GW_JAR:$JIEBA:$CONFIG_CP" \
  -d "$BUILD_DIR/classes" \
  src/main/scala/websearch/search/*.scala

echo "==> 解包 jieba-analysis 入 classes（fat jar——child-first 加载器自洽）"
JAR_ABS="$PWD/lib/jieba-analysis-1.0.2.jar"
(cd "$BUILD_DIR/classes" && unzip -oq "$JAR_ABS" "com/*" "dict.txt" "prob_emit.txt")
python3 - "$BUILD_DIR/classes" "$BUILD_DIR/config-libs" <<'PACK'
from pathlib import Path, PurePosixPath
import sys, zipfile
classes = Path(sys.argv[1])
for source in [Path('THIRD_PARTY.md'), *sorted(Path('licenses').glob('*.txt'))]:
    target = classes / 'META-INF/licenses' / source.name
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(source.read_bytes())
for name in ['Apache-2.0.txt', 'ANTLR-BSD-3-Clause.txt']:
    if not (classes / 'META-INF/licenses' / name).is_file():
        raise SystemExit('missing dependency license: ' + name)
for dependency in sorted(Path(sys.argv[2]).glob('*.jar')):
    with zipfile.ZipFile(dependency) as jar:
        for name in jar.namelist():
            path = PurePosixPath(name)
            if path.is_absolute() or '..' in path.parts:
                raise SystemExit('unsafe dependency entry: ' + name)
            if name.endswith('/'):
                continue
            if name.startswith(('org/', 'javax/')):
                target = classes / name
            elif any(word in name.upper() for word in ('LICENSE', 'NOTICE')):
                target = classes / 'META-INF/licenses' / dependency.name / name.replace('/', '_')
            else:
                continue
            if target.exists():
                raise SystemExit('duplicate dependency entry: ' + name)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(jar.read(name))
for entry in ['org/tomlj/Toml.class', 'org/antlr/v4/runtime/Lexer.class', 'org/checkerframework/checker/nullness/qual/Nullable.class']:
    if not (classes / entry).is_file():
        raise SystemExit('missing parser dependency: ' + entry)
PACK

echo "==> 打 fat jar"
jar --create --file "$BUILD_DIR/websearch-jvm-1.0.0.jar" --date=2020-01-01T00:00:00Z -C "$BUILD_DIR/classes" .
mv "$BUILD_DIR/websearch-jvm-1.0.0.jar" target/websearch-jvm-1.0.0.jar
ls -la target/websearch-jvm-1.0.0.jar
echo "✓ apps/websearch/jvm/target/websearch-jvm-1.0.0.jar（含 jieba+词典）"
