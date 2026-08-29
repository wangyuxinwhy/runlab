#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux ]]; then
    echo "verify-linux.sh requires Linux" >&2
    exit 2
fi
if [[ "$(id -u)" -ne 0 ]]; then
    echo "verify-linux.sh requires root for the real runc lifecycle gate" >&2
    exit 2
fi
: "${RUNLAB_NATIVE_E2E_OCI_LAYOUT:?set RUNLAB_NATIVE_E2E_OCI_LAYOUT to a deterministic OCI Image Layout}"
test -f "$RUNLAB_NATIVE_E2E_OCI_LAYOUT/oci-layout"
command -v runc >/dev/null

cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --workspace --all-targets --locked
cargo test -p run_engine \
    native::linux_evidence::tests::real_proc_exit_monitor_survives_eight_way_process_churn \
    --locked -- --ignored --exact --test-threads=1
e2e_tests=(
    native::execution::tests::e2e::real_runc_lifecycle_capture_mounts_and_secrets
    native::execution::tests::e2e::real_runc_egress_network
    native::execution::tests::e2e::real_runc_termination_timeout_and_cancellation
    native::execution::tests::e2e::real_runc_multi_program_coordination
    native::execution::tests::e2e::real_runc_concurrent_invocations
    native::execution::tests::e2e::real_runc_runtime_failures
)
for e2e_test in "${e2e_tests[@]}"; do
    cargo test -p run_engine "$e2e_test" --locked -- --ignored --exact --test-threads=1
done
cargo package -p run_protocol --list --allow-dirty >/dev/null
cargo package -p run_engine --list --allow-dirty >/dev/null
cargo package -p runlab --list --allow-dirty >/dev/null
