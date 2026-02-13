#!/bin/bash
set -eox pipefail

# shellcheck source=ci/ci-tests-common.sh
source "$(dirname "$0")/ci-tests-common.sh"

ci_step_start "Test taproot cfg flag"
RUSTFLAGS="--cfg=taproot" cargo test --quiet --color always -p lightning
[ "$CI_MINIMIZE_DISK_USAGE" != "" ] && cargo clean
ci_step_end

ci_step_start "Test simple_close cfg flag"
RUSTFLAGS="--cfg=simple_close" cargo test --quiet --color always -p lightning
[ "$CI_MINIMIZE_DISK_USAGE" != "" ] && cargo clean
ci_step_end

ci_step_start "Test lsps1_service cfg flag"
RUSTFLAGS="--cfg=lsps1_service" cargo test --quiet --color always -p lightning-liquidity
[ "$CI_MINIMIZE_DISK_USAGE" != "" ] && cargo clean
ci_step_end

ci_step_start "Test peer_storage cfg flag"
RUSTFLAGS="--cfg=peer_storage" cargo test --quiet --color always -p lightning
ci_step_end

ci_print_summary
