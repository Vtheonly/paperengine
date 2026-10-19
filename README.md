# PaperEngine

PaperEngine is a Rust-first, local-first system for discovering scholarly work, prioritizing lawful full-text candidates, acquiring authorized files, and maintaining a durable, searchable research library.

## Status

The repository now has its first **computational kernel and bounded event-dispatch slice**. It is not yet a complete downloader or production research-library application. Do not mistake the in-memory demonstration path for an end-to-end acquisition pipeline.

Implemented now:

- A formal engine lifecycle contract and six-layer module boundaries.
- A 64-byte, 64-byte-aligned candidate hot record with cold metadata kept outside it.
- Deterministic integer scoring over the entire candidate frontier.
- A fixed-size Structure-of-Arrays score batch, with runtime-gated AVX2 and a scalar fallback.
- Bounded anytime steps that preserve the best decision found so far.
- A 64-byte event envelope and bounded SPSC ring. The API enforces one producer and one consumer; the slots are initialized once and no heap allocations occur in enqueue/dequeue.
- A bounded orchestrator that returns on backpressure and retains a pending event rather than silently dropping it or spinning.
- A small `paperengine rank` CLI that reads normalized numeric candidate rows from standard input and emits JSON Lines decision events.
- GitHub Actions formatting, Clippy, test, and release-build checks.
- A bounded HTTP(S) downloader with byte and time budgets, ETag/Last-Modified-guarded resume, strict Range/Content-Range checks, safe restart on ignored or changed ranges, and no-clobber publication after a complete transfer.
- Local HTTP fixture tests for interrupted/resumed bodies, 206 responses, ignored Range, changed objects, timeouts, size limits, and simulated ENOSPC.

Still **not implemented**: source/provider adapters, URL safety and cached link validation, policy-aware orchestration and source-specific rate limits, durable queues/leases/retries, PDF validation and quarantine, content-addressed and multi-volume storage, background integrity scrubbing, JATS/TEI extraction, real PDF text extraction, zstd text artifacts, Tantivy indexing/search, CPU affinity, Python supervision, and measured benchmark baselines. Existing GitHub issues track those as separate tasks. No issue is considered complete merely because the current kernel compiles.

## Resumable downloader

The library exposes an outer HTTP adapter through `paperengine::fetcher::stream::ResumableDownloader`. It streams to a sibling `.part` file, records ETag/Last-Modified metadata, and reuses partial bytes only when a usable validator and a matching `Content-Range` make continuation safe. If a server ignores Range or returns a changed object, the existing partial is discarded rather than appended to. Body size, request time, and total elapsed time are bounded. The destination path is published only after the transfer reaches EOF and advertised lengths have been checked; an existing destination is not overwritten.

Example:

```rust
use paperengine::fetcher::stream::{DownloadConfig, ResumableDownloader};

let downloader = ResumableDownloader::new(DownloadConfig::default())?;
let result = downloader
    .download("https://example.org/authorized-paper.pdf", "/library/paper.pdf")
    .await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The caller is responsible for validating source policy and authorization before requesting a URL. This adapter is not yet wired into the CLI or orchestrator, does not validate whether a payload is actually a PDF, and does not add content-addressed storage; issues #28 and #29 track those next layers. A failed transfer can be resumed by invoking `download` again with the same URL and destination. Resume support requires a strong ETag or Last-Modified validator; without one, a leftover partial is restarted from byte zero.

## Architecture boundary

The computational core consumes a pre-normalized in-memory frontier and emits a **recommendation event**. It does not open sockets, read or write files, access a database, consult a clock, or manufacture paper URLs. An outer worker must resolve the actual manifestation and candidate URL, enforce the source's current policy, and re-check authorization immediately before making each request.

A restriction flag is a defense-in-depth signal, not an authorization system. HTTP 403/429, CAPTCHA, login, quotas, or explicit source restrictions must be recorded and handled through documented authorized routes, never bypassed.

The formal lifecycle is documented in [Engine Core Architecture](docs/ENGINE_CORE_ARCHITECTURE.md), and the domain-specific decision is captured in [ADR 0001](docs/adr/0001-pure-computational-kernel.md).

## CLI input format

The current `rank` command consumes one candidate per line:

```text
paper_id,citation_count,venue_rank,year,estimated_size_kib,source_id,flags
```

All fields are numeric. The flags value can be decimal or hexadecimal. Empty lines and lines beginning with `#` are ignored.

Example:

```bash
printf '101,42,10,2024,512,1,1\n102,180,14,2025,256,2,7\n' \
  | cargo run --release -- rank --year 2026 --policy-version 1
```

Decision events are emitted as JSON Lines. Intermediate improvements may be followed by a final event for the selected candidate. These are policy decisions only; no network operation happens in this command.

## Verification

With stable Rust installed:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

GitHub Actions runs these checks on pushes to `main` and pull requests. This editing environment does not have `cargo` or `rustc` installed, so local compilation has not been performed. Check the live CI run before calling this slice verified.

## Repository workflow

Read [Requirements](docs/REQUIREMENTS.md), [Traceability](docs/TRACEABILITY.md), and the relevant ADRs before implementation. Keep work on `main` by default, change one issue-sized slice at a time, and do not close an issue until its acceptance criteria and relevant failure paths have been verified. Never claim speed, scale, source coverage, or production readiness without reproducible evidence.
