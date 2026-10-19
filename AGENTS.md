# Repository Agent Instructions

Read README.md, docs/REQUIREMENTS.md, docs/TRACEABILITY.md, docs/ENGINE_CORE_ARCHITECTURE.md, and relevant ADRs before coding. Read the GitHub issue and its acceptance criteria first.

Work on main by default. Keep each change to one issue-sized slice; do not implement the entire backlog in one pass. Do not create feature branches unless explicitly requested.

PaperEngine is a scholarly research-acquisition project. Do not import the unrelated Hyperion chess engine or chess algorithms.

The computational kernel must not perform network, filesystem, database, clock, or subprocess I/O. Normalize external data before it enters the kernel. Keep URLs, source payloads, identifiers with source context, and provenance in outer adapter/cold-storage records.

The core emits recommendations only. Outer workers must re-check source policy, authorization, and quota immediately before network access. Never bypass paywalls, logins, CAPTCHAs, quotas, 403/429 responses, or explicit source restrictions.

The event ring currently has an SPSC contract. Do not claim MPMC safety or add unsafe concurrent access without a new design, proof-oriented review, and stress tests. Do not claim CPU pinning unless affinity is actually set and verified. Do not claim real PDF extraction from byte-marker scanning.

Before marking an issue complete, run cargo fmt --all -- --check, cargo clippy --all-targets --all-features -- -D warnings, cargo test --all-features, and cargo build --release. Update the docs and traceability. Performance claims require reproducible benchmark evidence.
