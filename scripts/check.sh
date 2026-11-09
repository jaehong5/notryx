#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
source scripts/dev-env.sh
npm run build
npm run lint
npm test
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo run --locked -p notryx-protocol --bin generate-contracts -- --check
python3 scripts/check-boundaries.py
npm audit --audit-level=high
