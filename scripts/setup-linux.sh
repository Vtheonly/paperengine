#!/usr/bin/env bash
# PaperEngine Linux setup and self-check.
#
# Verifies required tools, builds the engine, runs the offline test suite, and prints a
# smoke-test command the user can copy. Requires: git, a Rust toolchain (rustup), and
# optionally a contact e-mail for polite API pools. No cloud account or paid service is
# needed for any of this.
set -euo pipefail

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "Checking required tools"
command -v git >/dev/null || { echo "missing: git (apt install git)"; exit 1; }
if ! command -v cargo >/dev/null; then
  echo "cargo not found; installing a minimal stable Rust toolchain via rustup…"
  curl --proto '=https' --tlsv1.2 --retry 5 --retry-connrefused --location --silent --show-error --fail \
    https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy,rustfmt
  # shellcheck disable=SC1091
  source "${HOME}/.cargo/env"
fi
echo "git:    $(git --version)"
echo "rustc:  $(rustc --version)"
echo "cargo:  $(cargo --version)"

say "Building PaperEngine (release)"
cargo build --release

say "Running offline verification (fmt, clippy, tests)"
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features

say "Smoke test: offline rank command"
printf '101,42,10,2024,512,1,1\n102,180,14,2025,256,2,7\n' \
  | ./target/release/paperengine rank --year 2026 --policy-version 1

say "Setup complete"
cat <<'EOF'

Next steps (all local, no cloud account required):

  # 1. Optional: create your configuration
  cp paperengine.toml.example paperengine.toml
  $EDITOR paperengine.toml          # set contact_email

  # 2. Discover metadata (downloads nothing)
  ./target/release/paperengine discover --source arxiv \
      --query "attention is all you need" --max-records 5 --out discovery.jsonl

  # 3. Acquire permitted open-access PDFs into an explicit library directory
  ./target/release/paperengine acquire --from discovery.jsonl \
      --library ./research-library --max-downloads 2

  # 4. Inspect the resulting library
  ./target/release/paperengine inspect --library ./research-library

  # 5. Optional: bounded live tests against real sources (small request budget)
  PAPERENGINE_LIVE_TESTS=1 cargo test --test live_sources -- --ignored --nocapture --test-threads=1

Offline fixture tests never touch the network. Live tests are opt-in and rate-limited.
EOF
