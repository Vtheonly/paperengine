# PaperEngine

PaperEngine is a Rust-first, local-first system for discovering scholarly work, prioritizing lawful full-text candidates, acquiring authorized files, and maintaining a durable, searchable research library.

## Status

The repository now has a **working end-to-end acquisition slice**: bounded metadata discovery from real scholarly APIs, a policy-gated downloader, structural PDF validation with quarantine, and content-addressed storage with provenance. It is not yet a complete production research-library application — extraction, indexing, and search are still separate roadmap slices.

Implemented now:

- A formal engine lifecycle contract and six-layer module boundaries.
- A 64-byte, 64-byte-aligned candidate hot record with cold metadata kept outside it.
- Deterministic integer scoring over the entire candidate frontier.
- A fixed-size Structure-of-Arrays score batch, with runtime-gated AVX2 and a scalar fallback.
- Bounded anytime steps that preserve the best decision found so far.
- A 64-byte event envelope and bounded SPSC ring; one producer, one consumer, no heap allocations in enqueue/dequeue.
- A bounded orchestrator that returns on backpressure and retains pending events.
- A bounded HTTP(S) downloader with validator-guarded resumption, early PDF magic-byte rejection, incremental BLAKE3/SHA-256 digests, size limits, and no-clobber publication.
- Structural PDF validation and quarantine (lopdf): signature, EOF, length, MIME, checksum, page-tree, catalog, and encryption checks, with redacted quarantine manifests.
- A **versioned source-adapter contract** with typed error taxonomy (403/429 are stop conditions, never challenges), per-source rate policies with request pacing, and a registry that refuses incompatible plugins.
- **Source adapters** using documented public APIs: OpenAlex, arXiv, Crossref, Unpaywall, and Europe PMC. Crossref is metadata-only by design; OpenAlex/Unpaywall resolve open-access locations; arXiv provides direct OA PDFs; Europe PMC adds `cursorMark` pagination, PMCID identifiers, and OA-subset PDF render links.
- Identifier normalization for DOI, arXiv (modern + legacy), PMID, and PMCID with equivalent-form tests.
- A bounded, download-free **discovery runner** and a CLI: `discover`, `lookup`, `acquire`, `extract`, `inspect`, `self-test`, and `rank`.
- **Content-addressed storage**: `objects/<sha256[0..2]>/<sha256>.pdf` published by atomic rename, provenance manifests, per-run JSONL acquisition reports, dedup, and a pre-request URL policy gate (scheme, credentials, host).
- Offline integration tests over a local HTTP fixture server, plus opt-in bounded live tests against the real APIs — including real PDF acquisitions through the production pipeline (arXiv verified; Europe PMC acquisition is 403-blocked on datacenter networks and records BLOCKED evidence).
- Validated configuration file (`paperengine.toml`) with unknown-key rejection and actionable budget errors.
- **Provenance-aware extraction (#31)**: real-parser PDF text and `/Info` metadata extraction over finalized objects, with a versioned extractor identity, per-object provenance records, content-addressed text artifacts (`extracted/`), bounded budgets, deterministic re-runs, and an `extract` CLI command.
- GitHub Actions formatting, Clippy, test, and release-build checks; a separate manually-triggered live-test workflow.

Still **not implemented**: URL canonicalization and cached link revalidation, policy-aware durable queues/leases/retries, multi-volume storage and integrity scrubbing, JATS/TEI extraction, zstd text artifacts, Tantivy indexing/search, CPU affinity, Python supervision, and measured benchmark baselines. Existing GitHub issues track those as separate tasks. No issue is considered complete merely because the current kernel compiles.

## Source adapters

Twelve adapters (nine REST + three OAI-PMH instances) are implemented behind the versioned source contract (`src/sources/contract.rs`). Each was verified against its live API, and each one's claims are scoped to what the source actually provides:

| Source | Discovery | DOI lookup | Direct PDF | Verified |
|---|---|---|---|---|
| OpenAlex | yes (cursor pagination) | yes | via `best_oa_location.pdf_url` when present | live |
| arXiv | yes (Atom API) | – | yes (`/abs/…` → `/pdf/…`) | live + end-to-end acquisition |
| Crossref | yes (polite pool) | yes | never (metadata only) | live |
| Unpaywall | – | yes | yes (`url_for_pdf`) | live |
| Europe PMC | yes (REST search, `cursorMark` pagination) | yes | yes — API-reported OA render links (`europepmc.org/articles/…?pdf=render`); the website may 403 datacenter IPs (recorded, never bypassed) | live; acquisition BLOCKED on datacenter networks, pending unrestricted-network verification |
| PubMed Central | yes (E-utilities) | yes (`[PID]` exact match) | never — OA bulk utility observed 404 | live |
| DOAJ | yes (application API) | yes | yes (publisher-deposited `content_type=pdf` links) | live + end-to-end acquisition |
| Zenodo | yes (records API) | yes | yes (first `.pdf` file of publication records) | live + end-to-end acquisition |
| OSF Preprints | yes (JSON:API, `filter[title]`) | – | yes (resolved `primary_file` downloads) | live + end-to-end acquisition |
| OAI-PMH (Zenodo, DOAJ, PMC) | yes (`ListRecords` + `from` windows) | – | never — Dublin Core carries no full-text URLs | live |

The opt-in live suite (`PAPERENGINE_LIVE_TESTS=1`, plus `PAPERENGINE_LIVE_ACQUIRE=1` for real downloads) exercises every adapter against its real endpoint — all tests passed on 2026-10-19, including one real PDF acquisition per PDF-capable source with a verified path (arXiv, DOAJ, OSF, Zenodo) and an honestly recorded BLOCKED state for the Europe PMC render URL on datacenter networks. See `tests/live_sources.rs` and [docs/SOURCE_COVERAGE.md](docs/SOURCE_COVERAGE.md) for the per-source registry with dates, blockers, and honest capability statements.
## Quick start (Linux)

No cloud account, hosted database, or paid service is required.

```bash
git clone https://github.com/Vtheonly/paperengine.git
cd paperengine
./scripts/setup-linux.sh        # toolchain check, release build, offline tests, smoke run
```

Or manually:

```bash
cargo build --release
printf '101,42,10,2024,512,1,1\n' | ./target/release/paperengine rank --year 2026
```

### Configuration

Copy `paperengine.toml.example` to `paperengine.toml` and set `contact_email` (used for
polite API pools and required by Unpaywall). Every budget is validated; unknown keys are
rejected. The e-mail can also be passed with `--email` or `PAPERENGINE_CONTACT_EMAIL`.

### Discover metadata (no downloads)

```bash
./target/release/paperengine discover --source arxiv \
    --query "attention is all you need" \
    --max-records 5 --out discovery.jsonl
```

Sources: `arxiv`, `openalex`, `crossref`, `europepmc`. Discovery writes one normalized JSONL record
per work and structurally never downloads payload bytes.

### Resolve an open-access location for a DOI

```bash
./target/release/paperengine lookup --source unpaywall --doi 10.1371/journal.pone.0000219
```

### Acquire permitted open-access PDFs

```bash
./target/release/paperengine acquire --from discovery.jsonl \
    --library ./research-library --max-downloads 2
```

Each candidate is policy-checked immediately before its request (https-only by default,
no embedded credentials, host required), downloaded through the bounded resumable
downloader, structurally validated, and — when valid — finalized into content-addressed
storage with a provenance manifest. Invalid payloads are quarantined with a typed reason.
Run reports are written to `research-library/reports/`.

### Inspect the library

```bash
./target/release/paperengine inspect --library ./research-library
```

### Extract text and metadata from acquired objects

```bash
./target/release/paperengine extract --library ./research-library
```

Extraction runs the real PDF parser over every finalized object (no network), writes a
content-addressed text artifact under `extracted/`, and records provenance under
`extraction/`: extractor identity and version, source object digest, page count, PDF
`/Info` metadata (title, author, …), text hash and byte length, truncation flags, and
bounded per-page notes. Completed extractions are skipped on re-run; `--force`
re-extracts and produces byte-identical text. Failures are retried on the next run.
A per-run report lands in `research-library/reports/`.

### Recovery

`acquire` is restartable: objects are content-addressed and never clobbered, identical
re-downloads deduplicate, and interrupted transfers leave at most a scratch file under
`tmp/` that the next run clears before retrying. Failed and quarantined items keep their
records in `reports/` and `quarantine/` respectively.

## Google Colab

`colab/PaperEngine_Colab.ipynb` is a version-controlled notebook that installs the Rust
toolchain, builds the engine at a pinned revision, configures budgets, runs a real
discovery and a bounded real PDF acquisition, inspects the report, and copies the whole
library to Google Drive. `scripts/colab-equivalent-run.sh` replays the notebook's shell
cells on a plain Linux box and is what was executed for verification (see
[docs/VERIFICATION.md](docs/VERIFICATION.md)); the Colab-specific Drive mount remains to
be exercised on a real Colab runtime.

## Testing

```bash
cargo test --all-features                                # offline only; no network
PAPERENGINE_LIVE_TESTS=1 cargo test --test live_sources -- --ignored --nocapture
PAPERENGINE_LIVE_TESTS=1 PAPERENGINE_LIVE_ACQUIRE=1 \
  cargo test --test live_sources -- --ignored --nocapture   # + one real arXiv PDF
```

Live tests are opt-in, use tiny request budgets, honor each source's rate policy, and
record per-source results (including BLOCKED states such as OpenAlex's shared free-tier
budget). See the [source coverage registry](docs/SOURCE_COVERAGE.md) for per-source
status, capabilities, limitations, and last live-verification dates, and
[docs/VERIFICATION.md](docs/VERIFICATION.md) for recorded evidence with sizes, hashes,
and commands.
## Resumable downloader

The library exposes an outer HTTP adapter through `paperengine::fetcher::stream::ResumableDownloader`. It streams to a sibling `.part` file, records ETag/Last-Modified metadata, and reuses partial bytes only when a usable validator and a matching `Content-Range` make continuation safe. If a server ignores Range or returns a changed object, the existing partial is discarded rather than appended to. Body size, request time, and total elapsed time are bounded.

The downloader checks for `%PDF-` within the first 1,024 representation bytes, including when that prefix crosses range or stream-chunk boundaries. It refuses to publish invalid-signature payloads and removes their partial artifacts. It computes BLAKE3 and SHA-256 incrementally; when resuming, the existing partial prefix is re-read in fixed 64-KiB chunks to rebuild hash state without loading the whole file into memory. The default minimum payload size is 1 KiB and the default maximum is 512 MiB. The signature gate is an early bandwidth guard; full structural validation and quarantine happen in `paperengine::fetcher::pdf_validation` and are wired into the `acquire` pipeline.

Example:

```rust
use paperengine::fetcher::stream::{DownloadConfig, ResumableDownloader};

let downloader = ResumableDownloader::new(DownloadConfig::default())?;
let result = downloader
    .download("https://example.org/authorized-paper.pdf", "/library/paper.pdf")
    .await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The caller is responsible for validating source policy and authorization before requesting a URL. A failed transfer can be resumed by invoking `download` again with the same URL and destination. Resume support requires a strong ETag or Last-Modified validator; without one, a leftover partial is restarted from byte zero.

## Architecture boundary

The computational core consumes a pre-normalized in-memory frontier and emits a **recommendation event**. It does not open sockets, read or write files, access a database, consult a clock, or manufacture paper URLs. An outer worker must resolve the actual manifestation and candidate URL, enforce the source's current policy, and re-check authorization immediately before making each request.

A restriction flag is a defense-in-depth signal, not an authorization system. HTTP 403/429, CAPTCHA, login, quotas, or explicit source restrictions must be recorded and handled through documented authorized routes, never bypassed.

The formal lifecycle is documented in [Engine Core Architecture](docs/ENGINE_CORE_ARCHITECTURE.md), and the domain-specific decision is captured in [ADR 0001](docs/adr/0001-pure-computational-kernel.md). The source-adapter contract is documented in [docs/SOURCE_COVERAGE.md](docs/SOURCE_COVERAGE.md) and `src/sources/contract.rs`.

## CLI input format (rank command)

The `rank` command consumes one candidate per line:

```text
paper_id,citation_count,venue_rank,year,estimated_size_kib,source_id,flags
```

All fields are numeric. The flags value can be decimal or hexadecimal. Empty lines and lines beginning with `#` are ignored.

Example:

```bash
printf '101,42,10,2024,512,1,1\n102,180,14,2025,256,2,7\n' \
  | cargo run --release -- rank --year 2026 --policy-version 1
```

Decision events are emitted as JSON Lines. These are policy decisions only; no network operation happens in this command.

## Verification

With stable Rust installed:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

GitHub Actions runs these checks on pushes to `main` and pull requests, plus a
manually-triggered live-source workflow. Recorded verification evidence — including a
real end-to-end arXiv PDF acquisition with byte counts, SHA-256 hashes, and storage
paths — is in [docs/VERIFICATION.md](docs/VERIFICATION.md).

## Sources

Built-in adapters (see `paperengine self-test` for a live capability report and
[docs/SOURCE_COVERAGE.md](docs/SOURCE_COVERAGE.md) for the per-source verification registry):

| Source | Discovery | DOI lookup | Direct OA PDFs | Notes |
|---|---|---|---|---|
| OpenAlex | yes | yes | no | metadata; optional API key; 429 free-tier budgets are stop conditions |
| Crossref | yes | yes | no | metadata only; polite pool via `--email` |
| Unpaywall | no | yes | yes | resolves Crossref DOIs to OA locations; requires `--email` |
| arXiv | yes | no | yes | Atom API; 3-second courtesy spacing; live-verified acquisition |
| Europe PMC | yes | yes | yes | REST API with `cursorMark` pagination; OA-subset PDF render URLs, which the europepmc.org website may serve 403 to datacenter IPs — recorded, never bypassed |

## Repository workflow

Read [Requirements](docs/REQUIREMENTS.md), [Traceability](docs/TRACEABILITY.md), and the relevant ADRs before implementation. Keep work on `main` by default, change one issue-sized slice at a time, and do not close an issue until its acceptance criteria and relevant failure paths have been verified. Never claim speed, scale, source coverage, or production readiness without reproducible evidence.
