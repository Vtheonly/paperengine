# Requirement Traceability

This map links requirements to the existing issue backlog and the current implementation evidence.

| Area | Implementation / Issue |
|---|---|
| Scope, lawful access, paper-only defaults | docs/REQUIREMENTS.md; issue #1; paper-only work-type defaults live in `src/sources/work_record.rs` (`WorkTypeFilter`) |
| Capacity model and benchmark plan | issue #2; no performance claim accepted without measurements |
| Architecture boundaries and ADRs | docs/ENGINE_CORE_ARCHITECTURE.md; docs/adr/0001-pure-computational-kernel.md; issue #3 |
| Validated config and resource budgets | issue #4 (partial); `src/config.rs` validates `paperengine.toml` (unknown-key rejection, actionable budget errors); acquisition budgets enforced in `src/library.rs` |
| Publication, manifestation, object, source, candidate URL, job, provenance schema | issue #5 (partial); `WorkRecord` + `ProvenanceManifest` + content-addressed object paths in `src/sources/work_record.rs` / `src/library.rs`; full schema reconciliation still open |
| Durable job states and retry taxonomy | issue #6; not yet complete — typed error taxonomy exists (`SourceError`), durable job records do not |
| Isolated acquisition strategy kernel and bounded event dispatch | issue #42 (complete); src/engine_architecture.rs; src/layers/*; src/concurrency/ring.rs; src/runtime/orchestrator.rs; src/cli/rank.rs |
| Versioned source plugin contract and lifecycle | issue #7 (partial-complete); `src/sources/contract.rs` (versioned trait, registry, self-test, rate policies); in-process plugins are not sandboxed against panics (documented) |
| Metadata normalization, query planner, paper-only classifier | issue #8 (partial); `src/sources/normalize.rs` (DOI/arXiv/PMID/year/title); issue #9 (partial); bounded discovery runner in `src/sources/runner.rs` + `paperengine discover`; full query planning (synonyms, citation expansion) still open; #10 partial via `WorkTypeFilter` |
| OpenAlex, Crossref, Unpaywall adapters | issue #11 (partial); `src/sources/openalex.rs`, `crossref.rs`, `unpaywall.rs`; fixture + live-verified 2026-10-19 (see docs/SOURCE_COVERAGE.md); incremental ingestion and citation expansion still open |
| PMC/Europe PMC, arXiv, DOAJ, Zenodo, OSF adapters | issue #12 (partial); arXiv complete through Layer 3; the others are not implemented (registry in docs/SOURCE_COVERAGE.md) |
| OAI-PMH harvester and repository-family adapters | issue #13; not yet complete |
| URL canonicalization, validation, policy and TTL | issues #14 through #23 (partial); pre-request policy gate in `src/library.rs` (`authorize_url`: scheme, credentials, host); canonicalization/caching/TTL still open |
| Durable queue, leases, retries, backpressure scheduler | issues #18 through #26; the SPSC ring remains volatile; run-scoped budgets and reports exist in `src/library.rs`; staged dry-run planning still open |
| Streaming fetch with validator-guarded resume, PDF signature gate and incremental hashes | issue #27 (complete); `src/fetcher/stream.rs`, `tests/downloader_tests.rs` |
| PDF validation and quarantine | issue #28 (complete, closed 2026-10-19); `src/fetcher/pdf_validation.rs`, `tests/pdf_validation_tests.rs` |
| Content-addressed storage and atomic finalization | issue #29 (partial); `src/library.rs` (objects/, manifests/, atomic rename, dedup, sync); multi-volume still open |
| Multi-volume capacity, integrity scans and backups | issue #30; not yet complete |
| Extraction, incremental index, CLI search | issues #31 through #33; `paperengine inspect` lists objects/manifests; text extraction and Tantivy search still open |
| Logs, metrics, fault injection and benchmarks | issues #34 through #36; JSONL run reports exist (`src/library.rs`); structured logs/metrics/benchmarks still absent — no performance claims made |
| Source coverage registry and live verification | docs/SOURCE_COVERAGE.md (per-source status, capabilities, limits, last live result); docs/VERIFICATION.md (recorded evidence, 2026-10-19) |
| Colab and optional external workflows | issue #38 (partial); `colab/PaperEngine_Colab.ipynb` + `scripts/colab-equivalent-run.sh` (Linux replay verified; live Colab runtime not yet exercised) |
| Bulk ingestion, external storage, multi-worker coordination, optional read-only search | issues #37, #39 through #41; deferred |

## Completion rule

An issue is complete only after its acceptance criteria and relevant failure paths are verified, the CI checks pass, and the documentation reflects actual behavior. The kernel + downloader + validation + source-adapter + acquisition slices do not establish indexing, extraction, or production readiness.
