#!/bin/bash
# ci/ci-tests-common.sh - Shared helpers for CI test scripts.
# Source this file; do not execute it directly.
# shellcheck disable=SC2002,SC2207

RUSTC_MINOR_VERSION=$(rustc --version | awk '{ split($2,a,"."); print a[2] }')

# Some crates require pinning to meet our MSRV even for our downstream users,
# which we do here.
# Further crates which appear only as dev-dependencies are pinned further down.
function PIN_RELEASE_DEPS {
	return 0 # Don't fail the script if our rustc is higher than the last check
}

PIN_RELEASE_DEPS # pin the release dependencies in our main workspace

# The backtrace v0.3.75 crate relies on rustc 1.82
[ "$RUSTC_MINOR_VERSION" -lt 82 ] && cargo update -p backtrace --precise "0.3.74" --quiet

# Starting with version 1.2.0, the `idna_adapter` crate has an MSRV of rustc 1.81.0.
[ "$RUSTC_MINOR_VERSION" -lt 81 ] && cargo update -p idna_adapter --precise "1.1.0" --quiet

export RUST_BACKTRACE=1

CI_STEP_START_TIME=""
CI_STEP_NAME=""
CI_SUMMARY=""

function ci_step_start {
	CI_STEP_NAME="$1"
	CI_STEP_START_TIME=$(date +%s)
	echo "::group::$CI_STEP_NAME"
}

function ci_step_end {
	local elapsed=$(( $(date +%s) - CI_STEP_START_TIME ))
	local mins=$(( elapsed / 60 ))
	local secs=$(( elapsed % 60 ))
	echo "::endgroup::"
	echo "TIMING: ${CI_STEP_NAME} completed in ${mins}m ${secs}s"
	CI_SUMMARY="${CI_SUMMARY}${mins}m ${secs}s  ${CI_STEP_NAME}\n"
}

function ci_print_summary {
	echo ""
	echo "===== TIMING SUMMARY ====="
	echo -e "$CI_SUMMARY"
	echo "=========================="
}
