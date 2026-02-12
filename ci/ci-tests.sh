#!/bin/bash
#shellcheck disable=SC2002,SC2207
set -eox pipefail

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

echo -e "\n\nChecking the workspace, except lightning-transaction-sync."
cargo check --quiet --color always

echo -e "\n\nTesting the workspace 10 times, except lightning-transaction-sync."
for i in $(seq 1 10); do
	echo -e "\n\nWorkspace test run $i/10"
	cargo test --quiet --color always
done

exit 0
