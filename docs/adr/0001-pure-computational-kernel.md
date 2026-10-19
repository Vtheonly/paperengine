# ADR 0001: Isolate a domain-specific acquisition policy kernel

- Status: Accepted for the first implementation slice
- Date: 2026-10-19

## Context

The vault defines an engine as an iterative state-transition loop over a compressed domain model, isolated from I/O. PaperEngine is a scholarly-acquisition system, not a chess engine. The supplied blueprint combines unrelated chess search code, network/storage infrastructure, and an acquisition-policy ranking idea.

## Decision

- Keep the computational core in a Rust library over a caller-owned, normalized candidate frontier.
- Keep provider parsing, URL resolution, source-policy checks, HTTP, filesystem, persistence, clocks, process management, and Python orchestration outside the core.
- Use a fixed 64-byte candidate record and deterministic integer scores over fixed-size SoA batches.
- Enable AVX2 only for full batches on a runtime-detected compatible CPU; maintain an equivalent portable scalar implementation.
- Expose bounded anytime steps with a resumable cursor and best-so-far decision.
- Use a fixed-size event envelope across a bounded SPSC ring. Enforce one producer and one consumer with a borrowing split API.
- Let the orchestrator return on backpressure and retain pending events rather than spin or silently drop work.
- Treat every output as a recommendation. I/O adapters must re-check actual access permission and source policy before each request.

## Alternatives rejected

- **Copy the chess kernel literally:** bitboards, move generation, mate search, and chess transposition entries do not model research-acquisition policy.
- **Run HTTP inside the hot loop:** makes core behavior dependent on remote latency, sockets, external errors, and request policy.
- **Call a plain arithmetic loop SIMD:** rejected; vector code must use real intrinsics/runtime dispatch or be labelled scalar.
- **Claim an SPSC ring is MPMC:** rejected because that guarantee requires a different algorithm and ownership model.
- **Pin to CPU 0 unconditionally:** rejected; core numbering, affinity permissions, containers, and platform support vary. Affinity must be optional, verified, and separately tested.
- **Claim extraction from raw PDF markers is complete:** rejected; PDF content streams require real parsing and fixtures.
- **Add generic transposition memory without evidence:** rejected until a repeated-state workload and cache benefit have been demonstrated.

## Consequences

The kernel, event envelope, SPSC queue, bounded orchestrator, and minimal stdin CLI now form a testable integration slice. It is not yet an end-to-end research acquisition product. Source adapters, validation, durable jobs, downloads, content-addressed and multi-volume storage, extraction, indexing, CPU affinity, Python supervision, and repeatable benchmarks remain separate work.
