# Requirements and Scope

## Mission
Build a Rust-first local engine for discovering scholarly literature and building a durable, validated, searchable research library from sources that permit the relevant access and automation.

## In scope
- Scholarly paper/article/preprint discovery, with configurable report support and paper-only defaults.
- Metadata ingestion from documented APIs, repository exports, and OAI-PMH.
- DOI, arXiv, PMID, title, author, venue, date, and version normalization while preserving raw source records.
- Candidate URL validation, bounded redirects, cached revalidation, and source-policy enforcement.
- Durable jobs, leases, heartbeats, bounded retries, backpressure, and crash recovery.
- Streaming/resumable transfers where permitted and supported.
- Payload validation, quarantine, content-addressed storage, integrity scans, and backups.
- PDF/XML/text extraction, incremental indexing, local search, CLI diagnostics, and reproducible benchmarks.

## Out of scope
- Circumventing paywalls, authentication, CAPTCHAs, access controls, source quotas, or blocks.
- Treating HTTP 403/429 or CAPTCHA as a challenge to evade.
- Unbounded crawling, hidden automation against prohibited sources, or pretending a source is supported before verification.
- Unverified performance/corpus-capacity claims.
- A mandatory multi-user service or cloud deployment in the initial release.

## Core invariants
1. Discovery-only mode never downloads payload bytes.
2. All network work is policy-aware, bounded, observable, and attributable to a source.
3. Global concurrency never overrides per-source quotas or rate limits.
4. Raw metadata and provenance are retained; conflicts are not silently overwritten.
5. Different versions/manifestations are not incorrectly collapsed into one file.
6. Job transitions and retry decisions are persisted and validated.
7. Partial downloads never appear as finalized library objects.
8. Invalid or suspicious payloads are quarantined and excluded from search until verified.
9. Cache entries include policy/version context and are invalidated when relevant policy changes.
10. Every benchmark claim records environment, workload, configuration, and reproducible command.

## Acceptance expectations
- Typed configuration rejects invalid or unsafe resource limits with actionable errors.
- Adapter contract tests cover pagination, rate limits, errors, and restart/resume behavior.
- URL tests cover redirect loops, unsafe schemes, signed URL preservation, status/MIME classification, and timeouts.
- State-machine tests cover invalid transitions, restart recovery, lease expiry, and bounded retries.
- Storage tests cover interrupted writes, checksum mismatch, duplicate objects, and atomic finalization.
- Search tests cover incremental updates and deletion/reindex behavior.
- Load and fault-injection tests demonstrate bounded resource use and correct recovery.

## Open decisions
Record and resolve through ADRs: database/queue implementation, full-text index, hash algorithm, filesystem layout, plugin isolation model, concurrency runtime, OCR strategy, and supported platforms. Mark provisional choices as provisional rather than presenting them as settled.
