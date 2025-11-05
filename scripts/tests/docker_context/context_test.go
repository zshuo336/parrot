package contextcontracts

import (
	"github.com/moby/patternmatcher"
	"github.com/moby/patternmatcher/ignorefile"
	"os"
	"path/filepath"
	"testing"
)

// Dockerfile-specific files take precedence. Use Docker's own matcher rather
// than approximating its **, parent and negation rules in a separate parser.
func matcher(t *testing.T, dockerfile string) *patternmatcher.PatternMatcher {
	t.Helper()
	root := filepath.Join("..", "..", "..")
	if selected := os.Getenv("PARROT_CONTEXT_ROOT"); selected != "" {
		root = selected
	}
	path := filepath.Join(root, dockerfile+".dockerignore")
	if _, err := os.Stat(path); os.IsNotExist(err) {
		path = filepath.Join(root, ".dockerignore")
	}
	file, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	patterns, err := ignorefile.ReadAll(file)
	if err != nil {
		t.Fatal(err)
	}
	result, err := patternmatcher.New(patterns)
	if err != nil {
		t.Fatal(err)
	}
	return result
}

func TestJvmRuntimeArtifactsReachBuildContext(t *testing.T) {
	m := matcher(t, "deploy/images/gw-jvm.Dockerfile")
	for _, path := range []string{"interop/jvm/target/parrot-protocol-jvm-0.1.0.jar", "gw-jvm-libs.tar"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || excluded {
				t.Fatalf("required COPY source excluded: %s (%v)", path, err)
			}
		})
	}
}

func TestNodeSourceDependenciesReachBuildContext(t *testing.T) {
	m := matcher(t, "deploy/Dockerfile")
	for _, path := range []string{"Cargo.toml", "Cargo.lock", "crates/parrot-node/src/main.rs",
		"apps/websearch/Cargo.toml", "tools/parrot-obs/Cargo.toml",
		"vendor/rumqttc/Cargo.toml", "vendor/rumqttc/src/lib.rs",
		"vendor/tokio/Cargo.toml", "vendor/tokio/src/lib.rs",
		"vendor/tokio/src/loom/std/parking_lot.rs", "vendor/tokio/src/runtime/blocking/pool.rs",
		"vendor/tokio/src/runtime/context/runtime.rs", "vendor/tokio/src/runtime/park.rs"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || excluded {
				t.Fatalf("required node source excluded: %s (%v)", path, err)
			}
		})
	}
}

func TestJvmContextDoesNotShipOtherBuildOutputs(t *testing.T) {
	m := matcher(t, "deploy/images/gw-jvm.Dockerfile")
	for _, path := range []string{".git/config", ".env", "docs/review/archive.tar", "target/release/websearch",
		"interop/jvm/target/test-classes/Fixture.class", "interop/jvm/target/other.jar",
		"interop/jvm/src/main/scala/Example.scala", "apps/websearch/jvm/target/app.jar", "node_modules/pkg/index.js"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || !excluded {
				t.Fatalf("unrelated input included: %s (%v)", path, err)
			}
		})
	}
}

func TestAppReleaseArtifactsReachBuildContext(t *testing.T) {
	m := matcher(t, "deploy/images/gw-app.Dockerfile")
	for _, path := range []string{"target/aarch64-unknown-linux-musl/release/websearch",
		"target/x86_64-unknown-linux-musl/release/custom-tool", "target/release/crawler-lab", "deploy/images/validate_app_binary.py"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || excluded {
				t.Fatalf("required release binary excluded: %s (%v)", path, err)
			}
		})
	}
}

func TestAppContextExcludesDebugAndNestedCompilerOutputs(t *testing.T) {
	m := matcher(t, "deploy/images/gw-app.Dockerfile")
	for _, path := range []string{".git/config", ".env", "docs/review/archive.tar", "target/debug/websearch",
		"target/release/deps/libfoo.rlib", "target/aarch64-unknown-linux-musl/release/deps/libfoo.rlib",
		"target/aarch64-unknown-linux-musl/release/build/foo/output", "target/aarch64-unknown-linux-musl/debug/websearch", "deploy/images/prepare_jvm_libs.py", "deploy/private.env"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || !excluded {
				t.Fatalf("unrelated input included: %s (%v)", path, err)
			}
		})
	}
}

func TestRayRuntimeInputsReachBuildContext(t *testing.T) {
	m := matcher(t, "deploy/images/gw-ray.Dockerfile")
	for _, path := range []string{"deploy/images/install-ray.sh", "deploy/images/requirements-ray-build.lock",
		"deploy/images/requirements-ray.lock", "interop/python/parrot_protocol/ray_gw.py", "interop/python/parrot_protocol/wire.py"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || excluded {
				t.Fatalf("required runtime input excluded: %s (%v)", path, err)
			}
		})
	}
}

func TestRayContextExcludesUnrelatedInputs(t *testing.T) {
	m := matcher(t, "deploy/images/gw-ray.Dockerfile")
	for _, path := range []string{".git/config", ".env", "docs/review/archive.tar", "target/release/websearch",
		"interop/python/tests/test_wire_contracts.py", "interop/python/parrot_protocol/__pycache__/wire.cpython-311.pyc",
		"deploy/images/validate_app_binary.py", "apps/websearch/python/tokenizer.py"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || !excluded {
				t.Fatalf("unrelated input included: %s (%v)", path, err)
			}
		})
	}
}
