#!/bin/bash
set -eox pipefail

# shellcheck source=ci/ci-tests-common.sh
source "$(dirname "$0")/ci-tests-common.sh"

ci_step_start "Test lightning-block-sync with rest-client"
cargo test -p lightning-block-sync --quiet --color always --features rest-client
cargo check -p lightning-block-sync --quiet --color always --features rest-client
ci_step_end

ci_step_start "Test lightning-block-sync with rpc-client"
cargo test -p lightning-block-sync --quiet --color always --features rpc-client
cargo check -p lightning-block-sync --quiet --color always --features rpc-client
ci_step_end

ci_step_start "Test lightning-block-sync with rpc-client,rest-client"
cargo test -p lightning-block-sync --quiet --color always --features rpc-client,rest-client
cargo check -p lightning-block-sync --quiet --color always --features rpc-client,rest-client
ci_step_end

ci_step_start "Test lightning-block-sync with rpc-client,rest-client,tokio"
cargo test -p lightning-block-sync --quiet --color always --features rpc-client,rest-client,tokio
cargo check -p lightning-block-sync --quiet --color always --features rpc-client,rest-client,tokio
ci_step_end

ci_step_start "Check lightning-transaction-sync feature combos"
cargo check -p lightning-transaction-sync --quiet --color always --features esplora-blocking
cargo check -p lightning-transaction-sync --quiet --color always --features esplora-async
cargo check -p lightning-transaction-sync --quiet --color always --features esplora-async-https
cargo check -p lightning-transaction-sync --quiet --color always --features electrum
ci_step_end

if [ -z "$CI_ENV" ] && [[ -z "$BITCOIND_EXE" || -z "$ELECTRS_EXE" ]]; then
	ci_step_start "Check lightning-transaction-sync tests (no bitcoind)"
	cargo check -p lightning-transaction-sync --tests
	ci_step_end
else
	ci_step_start "Test lightning-transaction-sync with esplora-blocking"
	cargo test -p lightning-transaction-sync --quiet --color always --features esplora-blocking
	ci_step_end

	ci_step_start "Test lightning-transaction-sync with esplora-async"
	cargo test -p lightning-transaction-sync --quiet --color always --features esplora-async
	ci_step_end

	ci_step_start "Test lightning-transaction-sync with esplora-async-https"
	cargo test -p lightning-transaction-sync --quiet --color always --features esplora-async-https
	ci_step_end

	ci_step_start "Test lightning-transaction-sync with electrum"
	cargo test -p lightning-transaction-sync --quiet --color always --features electrum
	ci_step_end
fi

ci_print_summary
