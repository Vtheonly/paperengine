//! Bounded any-time controller over a caller-owned immutable frontier.

use core::fmt;
use super::layer2_state::FrontierNodeHot;
use super::layer3_transition::transition_batch;
use super::layer4_optimization::avx2_available;
use super::layer6_output::{AcquisitionDecision, OutputError, OutputInterpreter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyConfig {
    pub reference_year: u16,
    pub policy_version: u32,
    pub enable_simd: bool,
}
impl PolicyConfig {
    pub fn new(reference_year: u16, policy_version: u32) -> Result<Self, EngineError> {
        if !(1800..=9999).contains(&reference_year) {
            return Err(EngineError::InvalidReferenceYear(reference_year));
        }
        Ok(Self { reference_year, policy_version, enable_simd: true })
    }
    pub const fn with_simd(mut self, enabled: bool) -> Self {
        self.enable_simd = enabled;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchProgress {
    pub candidates_evaluated: u64,
    pub total_candidates: usize,
    pub complete: bool,
    pub best: Option<AcquisitionDecision>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    InvalidReferenceYear(u16),
    CandidateBudgetMustBePositive,
    CursorDoesNotMatchFrontier,
    Output(OutputError),
}
impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "engine error: {self:?}")
    }
}
impl std::error::Error for EngineError {}
impl From<OutputError> for EngineError {
    fn from(error: OutputError) -> Self { Self::Output(error) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineCursor {
    next_index: usize,
    total_candidates: usize,
    policy_version: u32,
    candidates_evaluated: u64,
    complete: bool,
    best: Option<AcquisitionDecision>,
}
impl EngineCursor {
    fn new(total_candidates: usize, policy_version: u32) -> Self {
        Self {
            next_index: 0, total_candidates, policy_version, candidates_evaluated: 0,
            complete: total_candidates == 0, best: None,
        }
    }
    pub const fn is_complete(&self) -> bool { self.complete }
    pub const fn best(&self) -> Option<AcquisitionDecision> { self.best }
    pub const fn candidates_evaluated(&self) -> u64 { self.candidates_evaluated }
}

/// Pure policy engine: no I/O resources, clock, or scheduler are embedded in this type.
#[derive(Clone, Copy, Debug)]
pub struct AcquisitionEngine {
    config: PolicyConfig,
    use_simd: bool,
}
impl AcquisitionEngine {
    pub fn new(config: PolicyConfig) -> Result<Self, EngineError> {
        if !(1800..=9999).contains(&config.reference_year) {
            return Err(EngineError::InvalidReferenceYear(config.reference_year));
        }
        Ok(Self { config, use_simd: config.enable_simd && avx2_available() })
    }
    pub const fn config(&self) -> PolicyConfig { self.config }
    pub const fn simd_enabled(&self) -> bool { self.use_simd }
    pub fn begin(&self, frontier: &[FrontierNodeHot]) -> EngineCursor {
        EngineCursor::new(frontier.len(), self.config.policy_version)
    }

    /// Evaluate at most candidate_budget records. The cursor holds a best-so-far decision
    /// after every completed call, enabling cooperative yielding at the adapter boundary.
    pub fn step(
        &self,
        frontier: &[FrontierNodeHot],
        cursor: &mut EngineCursor,
        candidate_budget: usize,
    ) -> Result<SearchProgress, EngineError> {
        if candidate_budget == 0 {
            return Err(EngineError::CandidateBudgetMustBePositive);
        }
        if cursor.total_candidates != frontier.len()
            || cursor.policy_version != self.config.policy_version
            || cursor.next_index > frontier.len()
        {
            return Err(EngineError::CursorDoesNotMatchFrontier);
        }
        if !cursor.complete {
            let stop_at = cursor.next_index.saturating_add(candidate_budget).min(frontier.len());
            while cursor.next_index < stop_at {
                let end = cursor.next_index
                    .saturating_add(super::layer4_optimization::SCORE_BATCH_SIZE)
                    .min(stop_at);
                transition_batch(
                    &frontier[cursor.next_index..end],
                    cursor.next_index,
                    self.config.reference_year,
                    self.config.policy_version,
                    self.use_simd,
                    &mut cursor.best,
                );
                let processed = end - cursor.next_index;
                cursor.next_index = end;
                cursor.candidates_evaluated =
                    cursor.candidates_evaluated.saturating_add(processed as u64);
            }
            cursor.complete = cursor.next_index == frontier.len();
        }
        Ok(SearchProgress {
            candidates_evaluated: cursor.candidates_evaluated,
            total_candidates: cursor.total_candidates,
            complete: cursor.complete,
            best: cursor.best,
        })
    }

    pub fn run_to_completion(
        &self,
        frontier: &[FrontierNodeHot],
        candidate_budget_per_step: usize,
    ) -> Result<Option<AcquisitionDecision>, EngineError> {
        let mut cursor = self.begin(frontier);
        while !cursor.is_complete() {
            self.step(frontier, &mut cursor, candidate_budget_per_step)?;
        }
        match cursor.best() {
            None => Ok(None),
            Some(decision) => Ok(Some(OutputInterpreter::validate_projection(
                frontier, decision, self.config.reference_year, self.config.policy_version,
            )?)),
        }
    }
}

impl crate::engine_architecture::EngineArchitecture<
    EngineCursor, (), Option<AcquisitionDecision>
> for AcquisitionEngine {
    fn initialize_state(
        &self, frontier: &[FrontierNodeHot], _context: &(),
    ) -> Result<EngineCursor, EngineError> {
        Ok(self.begin(frontier))
    }
    fn transition(
        &self, state: &mut EngineCursor, frontier: &[FrontierNodeHot],
        _context: &(), candidate_budget: usize,
    ) -> Result<(), EngineError> {
        self.step(frontier, state, candidate_budget)?;
        Ok(())
    }
    fn is_terminal(&self, state: &EngineCursor, _context: &()) -> bool {
        state.is_complete()
    }
    fn project_result(
        &self, state: &EngineCursor, frontier: &[FrontierNodeHot], _context: &(),
    ) -> Result<Option<AcquisitionDecision>, EngineError> {
        match state.best() {
            None => Ok(None),
            Some(decision) => Ok(Some(OutputInterpreter::validate_projection(
                frontier, decision, self.config.reference_year, self.config.policy_version,
            )?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::layer2_state::{
        FLAG_HAS_METADATA, FLAG_RESTRICTED_BY_SOURCE, FLAG_HAS_CANDIDATE_URL,
    };
    use crate::layers::layer6_output::AcquisitionAction;

    fn node(id: u64, citations: u32, flags: u32) -> FrontierNodeHot {
        FrontierNodeHot {
            paper_id: id, citation_count: citations, venue_rank: 2, year: 2024,
            flags, ..FrontierNodeHot::default()
        }
    }
    fn engine() -> AcquisitionEngine {
        AcquisitionEngine::new(PolicyConfig::new(2026, 1).unwrap()).unwrap()
    }

    #[test]
    fn any_time_steps_scan_the_entire_frontier() {
        let mut frontier: Vec<_> = (1..=40).map(|id| node(id, 1, FLAG_HAS_METADATA)).collect();
        frontier[33].citation_count = 99_999;
        let eng = engine();
        let mut cursor = eng.begin(&frontier);
        let first = eng.step(&frontier, &mut cursor, 16).unwrap();
        assert!(!first.complete);
        assert_eq!(first.candidates_evaluated, 16);
        while !cursor.is_complete() { eng.step(&frontier, &mut cursor, 16).unwrap(); }
        assert_eq!(cursor.best().unwrap().id.paper_id, 34);
        assert_eq!(cursor.candidates_evaluated(), 40);
    }

    #[test]
    fn restricted_candidate_never_wins() {
        let frontier = [
            node(1, 1_000, FLAG_HAS_METADATA),
            node(2, 100_000, FLAG_HAS_METADATA | FLAG_RESTRICTED_BY_SOURCE),
        ];
        assert_eq!(engine().run_to_completion(&frontier, 16).unwrap().unwrap().id.paper_id, 1);
    }

    #[test]
    fn acquisition_requires_authorized_url_and_pdf_gate() {
        let frontier = [node(1, 5, FLAG_HAS_METADATA | FLAG_HAS_CANDIDATE_URL)];
        let result = engine().run_to_completion(&frontier, 16).unwrap().unwrap();
        assert_eq!(result.action, AcquisitionAction::ResolveCandidateRoute);
    }

    #[test]
    fn empty_frontier_has_no_decision() {
        assert_eq!(engine().run_to_completion(&[], 16).unwrap(), None);
    }

    #[test]
    fn rejects_zero_budget_and_policy_mismatch() {
        let frontier = [node(1, 1, FLAG_HAS_METADATA)];
        let eng = engine();
        let mut cursor = eng.begin(&frontier);
        assert_eq!(eng.step(&frontier, &mut cursor, 0), Err(EngineError::CandidateBudgetMustBePositive));
        let other = AcquisitionEngine::new(PolicyConfig::new(2026, 2).unwrap()).unwrap();
        assert_eq!(other.step(&frontier, &mut cursor, 1), Err(EngineError::CursorDoesNotMatchFrontier));
    }
}
