#!/usr/bin/env bash
# Run inside an isolated container with a read-only frozen /workspace/source.
set -euo pipefail
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 CARGO_TERM_COLOR=never
export CARGO_NET_OFFLINE=true RUST_TEST_THREADS=4 PYTHONDONTWRITEBYTECODE=1
export RUSTFLAGS='-C link-arg=-Wl,--threads=1'
export RUSTDOCFLAGS='-C link-arg=-Wl,--threads=1'
export RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER=
OUT=/workspace/results
mkdir -p "$OUT"
test ! -e "$OUT/stages.tsv"
FAILED=0
run_stage() {
    local name=$1 code=0
    shift
    timeout 3600 "$@" >"$OUT/$name.log" 2>&1 || code=$?
    printf '%s\t%s\n' "$name" "$code" >>"$OUT/stages.tsv"
    printf '%s exit=%s\n' "$name" "$code"
    if [[ "$code" != 0 ]]; then FAILED=1; fi
}
cd /workspace/source
run_stage manifest sha256sum /workspace/manifest.json
run_stage input-before python3 scripts/project_inputs.py verify --root . --manifest /workspace/manifest.json --exact
if [[ "$FAILED" != 0 ]]; then exit "$FAILED"; fi
run_stage rustc bash -c 'rustc -Vv; printf "RUSTFLAGS=%s\nRUSTDOCFLAGS=%s\n" "$RUSTFLAGS" "$RUSTDOCFLAGS"; cat /sys/fs/cgroup/cpuset.cpus.effective'
# Only this execution copy is disposable; the frozen input remains read-only.
if [[ -e /tmp/parrot-linux-core-source ]]; then
    find /tmp/parrot-linux-core-source -depth -delete
fi
cp -a /workspace/source /tmp/parrot-linux-core-source
cd /tmp/parrot-linux-core-source
run_stage execution-input-before python3 scripts/project_inputs.py verify --root . --manifest /workspace/manifest.json --exact
if [[ "$FAILED" != 0 ]]; then exit "$FAILED"; fi
run_stage build-target python3 scripts/project_inputs.py build-target --root . --manifest /workspace/manifest.json --output /workspace/build-targets
if [[ "$FAILED" != 0 ]]; then exit "$FAILED"; fi
CARGO_TARGET_DIR=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["target_dir"])' "$OUT/build-target.log")
export CARGO_TARGET_DIR
tests=(--lib --test test_thread_actor_configuration --test test_native_instance_inputs
    --test test_thread_initialization_trace --test test_root_cohort
    --test test_root_cohort_host_exit --test test_runtime_root_cohort
    --test test_retirement_receipts --test test_shutdown_deadline
    --test test_individual_stop_deadline --test test_thread_supervision_host_exit
    --test test_reply_helper_ownership --test test_factory_retirement_contract
    --test test_factory_admission_contract --test test_child_factory_contract
    --test test_child_factory_inputs --test test_pending_factory_handoff
    --test test_typed_child_factory --test test_typed_factory
    --test test_typed_factory_creation_identity --test test_native_factory_inputs
    --test test_actix_root_factory_contract --test test_actix_child_factory_contract
    --test test_actix_typed_factory --test test_actix_factory_creation_identity
    --test test_local_factory --test test_local_factory_children
    --test test_actix_local_typed --test test_actix_typed_children
    --test test_actix_typed_lifecycle --test test_m4_typed_actix
    --test test_derived_local_runtime --test test_neutral_local
    --test test_neutral_local_children --test test_neutral_local_creation_identity
    --test test_local_children --test test_local_stream_init_identity
    --test test_local_stream_retirement)
for profile in debug release; do
    flags=()
    if [[ "$profile" == release ]]; then flags+=(--release); fi
    for feature in default legacy; do
        feature_flags=()
        if [[ "$feature" == legacy ]]; then feature_flags+=(--features parrot_envelope_legacy); fi
        run_stage "$profile-$feature" cargo test --target-dir "$CARGO_TARGET_DIR" --offline --locked --no-fail-fast "${flags[@]}" \
            -p parrot "${feature_flags[@]}" "${tests[@]}"
    done
done
run_stage api-library cargo test --target-dir "$CARGO_TARGET_DIR" --offline --locked -p parrot-api --lib
run_stage doctest cargo test --target-dir "$CARGO_TARGET_DIR" --offline --locked -p parrot --doc
run_stage clippy cargo clippy --target-dir "$CARGO_TARGET_DIR" --offline --locked -p parrot --lib --tests --all-features -- -D warnings
run_stage fmt cargo fmt --all -- --check
run_stage execution-input-after python3 scripts/project_inputs.py verify --root . --manifest /workspace/manifest.json --exact
run_stage input-after python3 scripts/project_inputs.py verify --root /workspace/source --manifest /workspace/manifest.json --exact
exit "$FAILED"
