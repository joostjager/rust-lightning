#!/bin/bash
set -euo pipefail

usage() {
	echo "Usage: $0 <target> <hex>"
	echo
	echo "Example:"
	echo "  $0 full_stack 2d31363837340901"
}

if [ "$#" -lt 2 ]; then
	usage >&2
	exit 2
fi

TARGET="${1%_target}"
shift
HEX="$(printf '%s' "$*" | tr -d '[:space:]')"

if [ -z "$HEX" ]; then
	echo "Missing hex input" >&2
	exit 2
fi

if [[ ! "$HEX" =~ ^[0-9a-fA-F]+$ ]]; then
	echo "Hex input contains non-hex characters" >&2
	exit 2
fi

if [ $((${#HEX} % 2)) -ne 0 ]; then
	echo "Hex input must have an even number of digits" >&2
	exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${TARGET}_target"

if [ -f "$SCRIPT_DIR/fuzz-fake-hashes/src/bin/$BIN.rs" ]; then
	MANIFEST="$SCRIPT_DIR/fuzz-fake-hashes/Cargo.toml"
	TARGET_RUSTFLAGS="--cfg=fuzzing --cfg=secp256k1_fuzz --cfg=hashes_fuzz"
elif [ -f "$SCRIPT_DIR/fuzz-real-hashes/src/bin/$BIN.rs" ]; then
	MANIFEST="$SCRIPT_DIR/fuzz-real-hashes/Cargo.toml"
	TARGET_RUSTFLAGS="--cfg=fuzzing --cfg=secp256k1_fuzz"
else
	echo "Unknown fuzz target: $TARGET" >&2
	exit 2
fi

printf '%s' "$HEX" | xxd -r -p | \
	RUST_BACKTRACE="${RUST_BACKTRACE:-1}" RUSTFLAGS="$TARGET_RUSTFLAGS" \
	cargo run --manifest-path "$MANIFEST" --features stdin_fuzz --bin "$BIN"
