#!/usr/bin/env bash
# Colab-equivalent verification: replays the notebook's bash cells (1-7) on a clean
# Linux checkout, proving the documented setup-and-run path without a Colab runtime.
# The Google Drive mount (cell 8) is Colab-specific and is exercised only on Colab.
set -euo pipefail

REVISION="${1:-main}"
WORKDIR="$(mktemp -d /tmp/paperengine-colab-equiv.XXXXXX)"
cd "$WORKDIR"
echo "workdir: $WORKDIR"

echo "== [cell 1] Rust toolchain =="
if ! command -v cargo >/dev/null; then
  curl --proto '=https' --tlsv1.2 --retry 5 --retry-connrefused \
    --location --silent --show-error --fail \
    https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy,rustfmt
fi
export PATH="$HOME/.cargo/bin:$PATH"
rustc --version && cargo --version

echo "== [cell 2] clone at identifiable revision =="
git clone -q https://github.com/Vtheonly/paperengine.git
cd paperengine
git fetch -q origin main
git checkout -q "$REVISION"
echo "BUILD_REVISION=$(git rev-parse HEAD)"

echo "== [cell 3] build and offline verification =="
cargo build --release
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features

echo "== [cell 4] configuration and self-test =="
cat > paperengine.toml <<'EOF'
schema_version = 1
contact_email = "paperengine@users.noreply.github.com"

[discovery]
default_page_size = 5
max_pages = 1
max_records = 5

[acquisition]
max_downloads = 2
max_bytes_per_file = 67108864
max_total_bytes = 134217728
EOF
./target/release/paperengine self-test

echo "== [cell 5] bounded live discovery (metadata only) =="
# One polite retry on transient transport failures (arXiv occasionally stalls a
# connection); anything the server answers — including 429/403 — is final.
discover_once() {
  ./target/release/paperengine discover \
      --source arxiv \
      --query "attention is all you need" \
      --max-records 5 \
      --out discovery.jsonl
}
if ! discover_once; then
  echo "transient discovery failure; one polite retry after 5s…"
  sleep 5
  discover_once
fi
echo "discovered records: $(wc -l < discovery.jsonl)"

echo "== [cell 5b] Unpaywall DOI resolution =="
./target/release/paperengine lookup --source unpaywall \
    --doi 10.1371/journal.pone.0000219

echo "== [cell 5c] Europe PMC discovery (OA subset, metadata only) =="
# Europe PMC's API (ebi.ac.uk) serves metadata from any network; the OA render PDFs on
# europepmc.org may be 403-blocked on datacenter IPs, so this step stays metadata-only.
./target/release/paperengine discover \
    --source europepmc \
    --query '"high-throughput sequencing" AND OPEN_ACCESS:Y' \
    --max-records 3 \
    --out epmc-discovery.jsonl
echo "europepmc records: $(wc -l < epmc-discovery.jsonl)"
./target/release/paperengine lookup --source europepmc \
    --doi 10.1371/journal.pone.0000219

echo "== [cell 6] real acquisition (bounded) =="
acquire_once() {
  ./target/release/paperengine acquire \
      --from discovery.jsonl \
      --library ./research-library \
      --max-downloads 2
}
if ! acquire_once; then
  echo "transient acquisition failure; one polite retry after 5s (--refetch overrides the job backoff)…"
  sleep 5
  ./target/release/paperengine acquire \
      --from discovery.jsonl \
      --library ./research-library \
      --max-downloads 2 \
      --refetch
fi

echo "== [cell 7] inspect library and report =="
./target/release/paperengine inspect --library ./research-library
latest_report=$(ls -1t research-library/reports/*.jsonl | head -n 1)
echo "latest report: $latest_report"
cat "$latest_report"

echo
echo "COLAB-EQUIVALENT RUN: SUCCESS (library in $WORKDIR/paperengine/research-library)"
