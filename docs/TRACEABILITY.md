# Requirement Traceability

This map links requirements to the existing issue backlog and the current implementation evidence.

| Area | Implementation / Issue |
|---|---|
| Scope, lawful access, paper-only defaults | docs/REQUIREMENTS.md; issue #1 |
| Capacity model and benchmark plan | issue #2; no performance claim accepted without measurements |
| Architecture boundaries and ADRs | docs/ENGINE_CORE_ARCHITECTURE.md; docs/adr/0001-pure-computational-kernel.md; issue #3 |
| Validated config and resource budgets | issue #4; not yet complete |
| Publication, manifestation, object, source, candidate URL, job, provenance schema | issue #5; not yet complete |
| Durable job states and retry taxonomy | issue #6; not yet complete |
| Isolated acquisition strategy kernel and bounded event dispatch | issue #42; src/engine_architecture.rs; src/layers/layer1_input.rs through layer6_output.rs; src/concurrency/ring.rs; src/runtime/orchestrator.rs; src/main.rs |
| Versioned source plugin contract and provider adapters | issues #7, #11, #12, #13; not yet complete |
| Metadata normalization, query planner, paper-only classifier | issues #8, #9, #10; kernel input normalization is only the primitive boundary |
| URL canonicalization, validation, policy and TTL | issues #14 through #23; not yet complete |
| Durable queue, leases, retries, backpressure scheduler | issues #24 through #26; current SPSC ring is volatile and is not a durable job queue |
| Streaming fetch with validator-guarded resume | issue #27; `src/fetcher/stream.rs` and local HTTP/fault tests; CI verified on main commit `a6125ce` |
| PDF validation and quarantine | issue #28; not yet complete |
| Content-addressed storage and atomic finalization | issue #29; not yet complete |
| Multi-volume capacity, integrity scans and backups | issue #30; not yet complete |
| Extraction, incremental index, CLI search | issues #31 through #33; not yet complete beyond the current candidate-ranking CLI |
| Logs, metrics, fault injection and benchmarks | issues #34 through #36; workflow checks are configured, but results must be verified and benchmark baselines are still absent |
| Bulk ingestion, external storage, multi-worker coordination, optional read-only search | issues #37 through #41; deferred |

## Completion rule

An issue is complete only after its acceptance criteria and relevant failure paths are verified, the CI checks pass, and the documentation reflects actual behavior. The kernel slice does not establish end-to-end production readiness or performance.
