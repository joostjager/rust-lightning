#!/bin/bash
#shellcheck disable=SC2002,SC2207
set -eox pipefail

# shellcheck source=ci/ci-tests-common.sh
source "$(dirname "$0")/ci-tests-common.sh"

echo -e "\n\nChecking the workspace, except lightning-transaction-sync."
cargo check --quiet --color always

WORKSPACE_MEMBERS=( $(cat Cargo.toml | tr '\n' '\r' | sed 's/\r    //g' | tr '\r' '\n' | grep '^members =' | sed 's/members.*=.*\[//' | tr -d '"' | tr ',' ' ') )

echo -e "\n\nTesting the workspace, except lightning-transaction-sync."
cargo test --quiet --color always

echo -e "\n\nTesting upgrade from prior versions of LDK"
pushd lightning-tests
cargo test --quiet
popd

echo -e "\n\nChecking and building docs for all workspace members individually..."
for DIR in "${WORKSPACE_MEMBERS[@]}"; do
	cargo check -p "$DIR" --quiet --color always
	cargo doc -p "$DIR" --quiet --document-private-items
done

echo -e "\n\nChecking and testing lightning with features"
cargo test -p lightning --quiet --color always --features dnssec
cargo check -p lightning --quiet --color always --features dnssec
cargo doc -p lightning --quiet --document-private-items --features dnssec

echo -e "\n\nChecking and testing lightning-persister with features"
cargo test -p lightning-persister --quiet --color always --features tokio
cargo check -p lightning-persister --quiet --color always --features tokio
cargo doc -p lightning-persister --quiet --document-private-items --features tokio

echo -e "\n\nTest Custom Message Macros"
cargo test -p lightning-custom-message --quiet --color always
[ "$CI_MINIMIZE_DISK_USAGE" != "" ] && cargo clean

echo -e "\n\nTest backtrace-debug builds"
cargo test -p lightning --quiet --color always --features backtrace
