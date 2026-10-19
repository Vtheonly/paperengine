# Requirement Traceability

This map connects project requirements to the initial phased GitHub backlog. Tests and implementation paths should be added as issues are completed.

| Area | Requirement | Initial issue |
|---|---|---|
| Scope and compliance | Freeze v1 scope, exclusions, lawful access boundaries | P0: Audit requirements and freeze v1 scope |
| Performance | Capacity model and reproducible benchmark plan | P0: Build capacity model and benchmark plan |
| Architecture | Explicit module boundaries and ADRs | P0: Define module boundaries and architecture decisions |
| Configuration | Typed config, safe resource budgets, secret redaction | P0: Implement validated configuration and resource budgets |
| Data | Work/manifestation/object/source/provenance separation | P0: Design publication and provenance schema |
| Reliability | Durable job states and error taxonomy | P0: Specify durable job state machine |
| Extensibility | Versioned source plugin lifecycle | P1: Define source plugin contract |
| Metadata | Stable identifier and metadata normalization | P1: Implement normalization |
| Discovery | Query planning and discovery-only workflow | P1: Build query planner |
| Classification | Paper-only defaults and auditable exclusions | P1: Implement paper-only filtering |
| Sources | Official APIs, repository adapters, OAI-PMH | P1: Source adapter issues |
| URLs | Canonicalization and bounded redirect tracking | P2: URL canonicalization |
| Link health | Tiered checks, status/MIME and caching | P2: Link-health validator and TTL |
| Policy | Politeness, quotas, restriction handling | P2: Source policy enforcement |
| Queue | Leases, heartbeats, recovery, adaptive scheduling | P3: Queue and scheduler issues |
| Acquisition | Streaming/resume, payload validation | P4: Downloader and PDF validation |
| Storage | Content addressing, atomic finalize, volume safety | P4: Storage issues |
| Extraction | Provenance-aware text and metadata extraction | P5: Extraction |
| Search | Incremental full-text and metadata index | P5: Indexing and CLI search |
| Operations | Structured logs, metrics, health checks | P6: Diagnostics |
| Quality | Fault injection, integration tests, benchmark gates | P6: Hardening |
| Optional extensions | Bulk ingestion, external storage, multi-worker, HTTP search | P7: Reviewed extension issues |

## Completion rule
An issue is complete only when acceptance criteria are tested, relevant invariants remain true, diagnostics explain failure modes, and documentation reflects actual behavior. Do not mark a feature complete based solely on compilation or a happy-path demo.
