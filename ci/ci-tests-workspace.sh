#!/bin/bash
#shellcheck disable=SC2002,SC2207
set -eox pipefail

# shellcheck source=ci/ci-tests-common.sh
source "$(dirname "$0")/ci-tests-common.sh"

ci_step_start "Check workspace"
cargo check --quiet --color always
ci_step_end

WORKSPACE_MEMBERS=( $(cat Cargo.toml | tr '\n' '\r' | sed 's/\r    //g' | tr '\r' '\n' | grep '^members =' | sed 's/members.*=.*\[//' | tr -d '"' | tr ',' ' ') )

ci_step_start "Test workspace"
cargo test --quiet --color always
ci_step_end

ci_step_start "Test LDK upgrade compatibility"
pushd lightning-tests
cargo test --quiet
popd
ci_step_end

ci_step_start "Check and build docs for all workspace members"
for DIR in "${WORKSPACE_MEMBERS[@]}"; do
	cargo check -p "$DIR" --quiet --color always
	cargo doc -p "$DIR" --quiet --document-private-items
done
ci_step_end

ci_step_start "Test lightning with dnssec feature"
cargo test -p lightning --quiet --color always --features dnssec
cargo check -p lightning --quiet --color always --features dnssec
cargo doc -p lightning --quiet --document-private-items --features dnssec
ci_step_end

ci_step_start "Test lightning-persister with tokio feature"
cargo test -p lightning-persister --quiet --color always --features tokio
cargo check -p lightning-persister --quiet --color always --features tokio
cargo doc -p lightning-persister --quiet --document-private-items --features tokio
ci_step_end

ci_step_start "Test lightning-custom-message"
cargo test -p lightning-custom-message --quiet --color always
[ "$CI_MINIMIZE_DISK_USAGE" != "" ] && cargo clean
ci_step_end

ci_step_start "Test lightning with backtrace feature"
cargo test -p lightning --quiet --color always --features backtrace
ci_step_end

ci_print_summary
