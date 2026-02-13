#!/bin/bash
set -eox pipefail

# shellcheck source=ci/ci-tests-common.sh
source "$(dirname "$0")/ci-tests-common.sh"

ci_step_start "Test c_bindings full workspace"
# Note that because `$RUSTFLAGS` is not passed through to doctest builds we cannot selectively
# disable doctests in `c_bindings` so we skip doctests entirely here.
RUSTFLAGS="$RUSTFLAGS --cfg=c_bindings" cargo test --quiet --color always --lib --bins --tests
ci_step_end

ci_step_start "Test c_bindings no-default-features per crate"
for DIR in lightning-invoice lightning-rapid-gossip-sync; do
	# check if there is a conflict between no_std and the c_bindings cfg
	RUSTFLAGS="$RUSTFLAGS --cfg=c_bindings" cargo test -p $DIR --quiet --color always --no-default-features
done

# Note that because `$RUSTFLAGS` is not passed through to doctest builds we cannot selectively
# disable doctests in `c_bindings` so we skip doctests entirely here.
RUSTFLAGS="$RUSTFLAGS --cfg=c_bindings" cargo test -p lightning-background-processor --quiet --color always --no-default-features --lib --bins --tests
RUSTFLAGS="$RUSTFLAGS --cfg=c_bindings" cargo test -p lightning --quiet --color always --no-default-features --lib --bins --tests
ci_step_end

ci_step_start "Test ldk_test_vectors and serde"
# Note that outbound_commitment_test only runs in this mode because of hardcoded signature values
RUSTFLAGS="$RUSTFLAGS --cfg=ldk_test_vectors" cargo test -p lightning --quiet --color always --no-default-features --features=std
# This one only works for lightning-invoice
# check that compile with no_std and serde works in lightning-invoice
cargo test -p lightning-invoice --quiet --color always --no-default-features --features serde
ci_step_end

ci_step_start "Test no-std-check downstream crate"
# check no-std compatibility across dependencies
pushd no-std-check
cargo check --quiet --color always
[ "$CI_MINIMIZE_DISK_USAGE" != "" ] && cargo clean
popd
ci_step_end

ci_step_start "Test msrv-no-dev-deps-check"
# Test that we can build downstream code with only the "release pins".
pushd msrv-no-dev-deps-check
PIN_RELEASE_DEPS
cargo check --quiet
[ "$CI_MINIMIZE_DISK_USAGE" != "" ] && cargo clean
popd
ci_step_end

if [ -f "$(which arm-none-eabi-gcc)" ]; then
	ci_step_start "Build no-std-check for ARM"
	pushd no-std-check
	cargo build --quiet --target=thumbv7m-none-eabi
	[ "$CI_MINIMIZE_DISK_USAGE" != "" ] && cargo clean
	popd
	ci_step_end
fi

ci_print_summary
