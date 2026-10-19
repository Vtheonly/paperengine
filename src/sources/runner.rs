//! Bounded discovery runner (#9 partial).
//!
//! Drives one adapter through cursor pagination under explicit budgets, streaming each page
//! through a caller callback so interrupted runs keep completed pages on disk. Discovery is
//! structurally download-free: this runner has no downloader and cannot fetch payload bytes.

use crate::sources::contract::{DiscoveryQuery, SourceAdapter, SourceError};
use crate::sources::work_record::WorkRecord;
use std::io;

/// Budgets bounding one discovery run.
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryBudget {
    /// Stop after this many records even if more pages exist.
    pub max_records: usize,
    /// Never fetch more than this many pages.
    pub max_pages: u32,
}

impl Default for DiscoveryBudget {
    fn default() -> Self {
        Self {
            max_records: 25,
            max_pages: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryRunSummary {
    pub source_id: String,
    pub records_returned: usize,
    pub pages_fetched: u32,
    /// True when the run stopped because of a budget, not source exhaustion.
    pub truncated: bool,
}

/// Run discovery, invoking `on_page` after each successful page. Pagination stops at the
/// first page that returns no records, when the source reports no further cursor, or when a
/// budget is exhausted.
pub async fn run_discovery<F>(
    adapter: &dyn SourceAdapter,
    query: &DiscoveryQuery,
    budget: &DiscoveryBudget,
    mut on_page: F,
) -> Result<DiscoveryRunSummary, SourceError>
where
    F: FnMut(&[WorkRecord]) -> Result<(), io::Error>,
{
    let mut records_returned = 0_usize;
    let mut pages_fetched = 0_u32;
    let mut cursor: Option<String> = None;
    let mut truncated = false;

    loop {
        if pages_fetched >= budget.max_pages {
            truncated = records_returned > 0;
            break;
        }
        if records_returned >= budget.max_records {
            truncated = true;
            break;
        }
        let page = adapter.discover(query, cursor.as_deref()).await?;
        pages_fetched += 1;
        let remaining = budget.max_records - records_returned;
        let available = page.records.len();
        let take = available.min(remaining);
        let drained: Vec<WorkRecord> = page.records.into_iter().take(take).collect();
        if drained.is_empty() {
            break;
        }
        on_page(&drained).map_err(|error| SourceError::OutputWrite {
            detail: error.to_string(),
        })?;
        records_returned += drained.len();
        if take < available {
            // The record budget cut this page short: more records existed than were
            // accepted, so the run is truncated regardless of any next cursor.
            truncated = true;
            break;
        }
        match page.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => break,
        }
    }

    Ok(DiscoveryRunSummary {
        source_id: adapter.descriptor().id.to_owned(),
        records_returned,
        pages_fetched,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::contract::{Capability, DiscoveryPage, RatePolicy, SelfTestReport};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Test adapter serving fixed pages with a numeric cursor.
    struct WithDescriptor {
        pages: Vec<Vec<WorkRecord>>,
        calls: AtomicUsize,
        descriptor: crate::sources::contract::SourceDescriptor,
    }

    #[async_trait]
    impl SourceAdapter for WithDescriptor {
        fn descriptor(&self) -> &crate::sources::contract::SourceDescriptor {
            &self.descriptor
        }

        fn self_test(&self) -> SelfTestReport {
            SelfTestReport {
                source_id: self.descriptor.id.to_owned(),
                adapter_version: 1,
                contract_version: 1,
                capabilities: vec![Capability::Discovery.as_str().to_owned()],
                configured: true,
                notes: Vec::new(),
            }
        }

        async fn discover(
            &self,
            _query: &DiscoveryQuery,
            cursor: Option<&str>,
        ) -> Result<DiscoveryPage, SourceError> {
            let index: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(call, index, "cursor must advance by page index");
            let records = self.pages.get(index).cloned().unwrap_or_default();
            let next = if index + 1 < self.pages.len() && !records.is_empty() {
                Some((index + 1).to_string())
            } else {
                None
            };
            Ok(DiscoveryPage {
                records,
                next_cursor: next,
            })
        }
    }

    fn record(id: u32) -> WorkRecord {
        let mut record = WorkRecord::new("paged", format!("r{id}"));
        record.title = format!("record {id}");
        record
    }

    fn descriptor() -> crate::sources::contract::SourceDescriptor {
        crate::sources::contract::SourceDescriptor {
            id: "paged",
            display_name: "Paged",
            adapter_version: 1,
            contract_version: 1,
            api_base: "https://example.org",
            access_basis: "test",
            capabilities: &[Capability::Discovery],
            rate_policy: RatePolicy {
                min_request_interval: Duration::from_millis(0),
                default_page_size: 2,
                max_page_size: 2,
            },
        }
    }

    fn adapter(pages: Vec<Vec<WorkRecord>>) -> WithDescriptor {
        WithDescriptor {
            pages,
            calls: AtomicUsize::new(0),
            descriptor: descriptor(),
        }
    }

    #[tokio::test]
    async fn pagination_stops_when_source_is_exhausted() {
        let adapter = adapter(vec![vec![record(1), record(2)], vec![record(3)]]);
        let mut seen = Vec::new();
        let summary = run_discovery(
            &adapter,
            &DiscoveryQuery::new("q"),
            &DiscoveryBudget {
                max_records: 100,
                max_pages: 10,
            },
            |page| {
                seen.extend(page.iter().map(|r| r.source_record_id.clone()));
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(seen, vec!["r1", "r2", "r3"]);
        assert_eq!(summary.records_returned, 3);
        assert_eq!(summary.pages_fetched, 2);
        assert!(!summary.truncated);
    }

    #[tokio::test]
    async fn record_budget_truncates_and_marks_summary() {
        let adapter = adapter(vec![vec![record(1), record(2)], vec![record(3), record(4)]]);
        let summary = run_discovery(
            &adapter,
            &DiscoveryQuery::new("q"),
            &DiscoveryBudget {
                max_records: 3,
                max_pages: 10,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(summary.records_returned, 3);
        assert!(summary.truncated);
    }

    #[tokio::test]
    async fn page_budget_limits_requests() {
        let adapter = adapter(vec![vec![record(1)], vec![record(2)], vec![record(3)]]);
        let summary = run_discovery(
            &adapter,
            &DiscoveryQuery::new("q"),
            &DiscoveryBudget {
                max_records: 100,
                max_pages: 2,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(summary.pages_fetched, 2);
        assert_eq!(summary.records_returned, 2);
        assert!(summary.truncated);
    }

    #[tokio::test]
    async fn empty_first_page_returns_zero_records_without_error() {
        let adapter = adapter(Vec::new());
        let mut seen = 0;
        let summary = run_discovery(
            &adapter,
            &DiscoveryQuery::new("q"),
            &DiscoveryBudget::default(),
            |_| {
                seen += 1;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(seen, 0);
        assert_eq!(summary.records_returned, 0);
        assert_eq!(summary.pages_fetched, 1);
        assert!(!summary.truncated);
    }
}
