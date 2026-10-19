# Engine Core Architecture

## Purpose

The implemented kernel ranks normalized scholarly-work candidates for future acquisition work. It does not crawl, download, parse PDFs, or serve a search index. Network, disk, persistence, and provider details live in outer adapters.

## Formal model

| Element | Concrete representation |
|---|---|
| State space S | Preloaded immutable frontier of `FrontierNodeHot` plus the mutable `EngineCursor` |
| Initial state s0 | Cursor at index zero with no current best decision |
| Transition F | Score a bounded batch and update deterministic best-so-far selection |
| Context C | Preloaded reference year, policy version, and runtime-approved SIMD mode |
| Termination tau | Every record in the same frontier snapshot has been examined |
| Projection rho | Validate and emit the selected recommendation |

The `EngineArchitecture` trait formalizes initialization, repeated transitions, a terminal predicate, and output projection. No core API accepts an HTTP client, file handle, database connection, clock, or subprocess.

## Six-layer implementation

1. **Input normalization — `layer1_input.rs`:** validates nonzero IDs, known flags, and plausible publication year ranges; constructs a fixed-size record.
2. **State representation — `layer2_state.rs`:** `FrontierNodeHot` is exactly 64 bytes and aligned to 64 bytes. URLs, source payloads, and provenance remain outside this record.
3. **Transition function — `layer3_transition.rs`:** evaluates fixed batches and selects the best candidate using a deterministic tie-break. Restricted candidates are ineligible.
4. **Optimization — `layer4_optimization.rs`:** fixed 16-lane SoA arrays; an AVX2 path is used only for full batches on runtime-detected compatible x86-64 CPUs. The scalar integer formula is the fallback.
5. **Control — `layer5_control.rs`:** bounded candidate budgets, a resumable cursor, deterministic best-so-far state, and explicit policy-version binding.
6. **Output interpretation — `layer6_output.rs`:** verifies index, identity, policy version, restriction state, action prerequisites, and score before projection.

## Scoring policy

The initial provisional score is:

```text
score =
    min(citation_count, 100000) * 4
  + min(venue_rank, 1000) * 128
  + recency_points(year, reference_year)
  - min(estimated_size_kib, 1000000) / 4
  + signal_bonuses
```

Unknown or future years receive no recency bonus. Known years receive `(100 - min(reference_year - year, 100)) * 64`. Signal bonuses are +120000 for the authorized-full-text signal, +240000 for a verified PDF signature, +30000 for JATS availability, and +6000 for metadata presence. These values are placeholder policy weights—not evidence of relevance quality or optimality. Version the policy and benchmark on a representative permitted corpus before tuning.

Scores are integer-valued and inputs are clamped so arithmetic remains within `i32`. The SIMD path must remain bit-for-bit identical to scalar scoring.

## Event boundary and loop

- `TaskEnvelope` is a 64-byte, 64-byte-aligned, pointer-free event. Its payload hash is an opaque key, not a URL or a token granting permission.
- `EngineRingBuffer<N>` preallocates its cells and exposes one producer and one consumer through a non-Clone SPSC split API. Slot access is private; release/acquire sequence ordering publishes completed events and returns slots safely.
- `EngineOrchestrator::run_turn` performs a bounded amount of computation, dispatches best-so-far changes, and retains an event if the ring is full. It returns to its caller instead of spinning while waiting for space.
- The current CLI demonstrates this event flow using normalized rows on stdin. It does not contain a real I/O worker and does not fabricate paper URLs.

This ring is specifically SPSC. It is **not MPMC**, and callers must not create extra producers/consumers by cloning or bypassing the split API.

## Safety and access policy

Decision flags may be stale or incomplete. The core emits recommendations only. Before any actual request, a future adapter must resolve the candidate URL and manifestation, enforce source policy, honor quotas and terms, and verify that access is allowed at that time. Restrictions, CAPTCHAs, authentication demands, 403/429, and explicit prohibitions are not challenges to bypass.

## Deferred engineering decisions

- **CPU affinity:** not implemented. Thread naming/logging does not pin a thread. Add affinity only as an explicit platform-specific option with clear errors and tests.
- **Extraction:** not implemented. Searching raw PDF bytes for `BT`/`ET` is not a correct PDF text extractor; content streams may be compressed, encoded, or segmented. Use a real parser and fixtures.
- **XML/JATS:** not implemented. Structured XML extraction must handle namespaces, nesting, entity decoding, and malformed input without panics.
- **zstd artifacts:** not implemented. Text outputs need versioned metadata/provenance and crash-safe finalization.
- **CAS/multi-volume storage:** not implemented. Use streaming hashing, validated hash paths, no-clobber finalization, sync semantics, capacity re-checks and integrity tests.
- **Tantivy indexer:** not implemented. A single writer and committed/indexed state must have durable semantics; timer-based commits need a real lifecycle.
- **Background scrubber:** not implemented. It needs a persisted object manifest, bounded I/O, cancellation, throttling and clear quarantine/repair behavior.
- **Python supervisor:** not implemented. It must use structured output, timeouts and graceful shutdown without claiming the workers have drained unless acknowledged.
- **Benchmarks:** not implemented. No throughput or nanosecond claims are accepted until reproducible measurements exist.

Do not add a generic chess-style transposition table to the acquisition ranker without showing a repeated-state workload and measured benefit. Do not port the unrelated chess engine into PaperEngine. Do not add a custom MPMC ring or unsupported performance claims.
