package contextcontracts

import "testing"

func TestDefaultContextExcludesLocalInputs(t *testing.T) {
	m := matcher(t, "Dockerfile")
	for _, path := range []string{".env", ".env.production", "apps/websearch/.env", "apps/websearch/.env.local",
		"docs/review/independent-2026-10-08/execution.log", "docs/review/evidence.tar.gz",
		"vendor/pkg/.git/config"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || !excluded {
				t.Fatalf("local input included in default context: %s (%v)", path, err)
			}
		})
	}
}

func TestNodeContextExcludesLocalInputs(t *testing.T) {
	m := matcher(t, "deploy/Dockerfile")
	for _, path := range []string{".env", "apps/websearch/.env.local", "crates/parrot-node/.env.production",
		"apps/websearch/target/release/websearch", "apps/websearch/node_modules/pkg/index.js",
		"apps/websearch/.scala-build/cache", "apps/websearch/python/__pycache__/tokenizer.pyc",
		"apps/websearch/erlang/_build/runtime.beam", "vendor/pkg/.git/config"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || !excluded {
				t.Fatalf("local input included in node source context: %s (%v)", path, err)
			}
		})
	}
}

func TestNodeContextExcludesUnrelatedSources(t *testing.T) {
	m := matcher(t, "deploy/Dockerfile")
	for _, path := range []string{".git/config", "docs/review/execution.log", "docs/review/evidence.tar.gz",
		"interop/erlang/parrot_gw.erl", "interop/jvm/target/runtime.jar", "gw-jvm-libs.tar",
		"deploy/parrot-stack/private.toml", "unrelated-secret.toml"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || !excluded {
				t.Fatalf("unrelated input included in node source context: %s (%v)", path, err)
			}
		})
	}
}

func TestErlangRuntimeSourcesReachBuildContext(t *testing.T) {
	m := matcher(t, "deploy/images/gw-erl.Dockerfile")
	for _, path := range []string{"interop/erlang/parrot_gw.erl", "interop/erlang/parrot_component_runtime.erl",
		"interop/erlang/parrot_beam_snapshot.erl", "interop/erlang/parrot_beam_guest.erl", "interop/erlang/parrot_beam_host.erl"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || excluded {
				t.Fatalf("required Erlang COPY source excluded: %s (%v)", path, err)
			}
		})
	}
}

func TestErlangContextExcludesUnrelatedInputs(t *testing.T) {
	m := matcher(t, "deploy/images/gw-erl.Dockerfile")
	for _, path := range []string{".git/config", ".env", "docs/review/execution.log", "docs/review/evidence.tar.gz",
		"Cargo.toml", "crates/parrot-node/src/main.rs", "apps/websearch/erlang/worker.beam",
		"interop/erlang/parrot_gw.beam", "interop/erlang/private.env", "interop/erlang/.env.local",
		"interop/erlang/unrelated.erl", "target/release/parrot-node"} {
		t.Run(path, func(t *testing.T) {
			excluded, err := m.MatchesOrParentMatches(path)
			if err != nil || !excluded {
				t.Fatalf("unrelated input included in Erlang context: %s (%v)", path, err)
			}
		})
	}
}
