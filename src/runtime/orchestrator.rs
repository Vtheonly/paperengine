//! Connects the pure in-memory acquisition kernel to a bounded I/O-worker event queue.
//!
//! This module dispatches decisions only. It does not invent URLs, perform HTTP, authorize
//! sources, or declare download/indexing completion. Worker adapters resolve the real paper
//! and source metadata, re-check policy, and then execute the requested operation.

use crate::concurrency::ring::{
    RingProducer, TaskEnvelope, ACTION_ACQUIRE_VERIFIED, ACTION_ENRICH_METADATA,
    ACTION_RESOLVE_ROUTE, ACTION_VALIDATE_CANDIDATE, EVENT_FLAG_FINAL,
};
use crate::layers::layer2_state::FrontierNodeHot;
use crate::layers::layer5_control::{AcquisitionEngine, EngineCursor, EngineError, SearchProgress};
use crate::layers::layer6_output::{AcquisitionAction, AcquisitionDecision};
use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    Engine(EngineError),
    CandidateIndexTooLarge(usize),
}
impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "runtime dispatch error: {self:?}")
    }
}
impl std::error::Error for RuntimeError {}
impl From<EngineError> for RuntimeError {
    fn from(error: EngineError) -> Self { Self::Engine(error) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeProgress {
    pub candidates_evaluated: u64,
    pub total_candidates: usize,
    pub completed: bool,
    pub backpressured: bool,
    pub best: Option<AcquisitionDecision>,
}

/// Persistent loop state, including one retained event if the ring is full.
/// The caller must keep passing the same immutable frontier for the cursor's lifetime.
pub struct OrchestratorCursor {
    engine_cursor: EngineCursor,
    last_sent: Option<AcquisitionDecision>,
    pending: Option<TaskEnvelope>,
    pending_decision: Option<AcquisitionDecision>,
    pending_final: bool,
    final_sent: bool,
}

impl OrchestratorCursor {
    pub fn new(engine: &AcquisitionEngine, frontier: &[FrontierNodeHot]) -> Self {
        Self {
            engine_cursor: engine.begin(frontier),
            last_sent: None,
            pending: None,
            pending_decision: None,
            pending_final: false,
            final_sent: false,
        }
    }

    pub const fn is_complete(&self) -> bool {
        self.final_sent
    }

    pub const fn best(&self) -> Option<AcquisitionDecision> {
        self.engine_cursor.best()
    }
}

/// A synchronous bounded event-loop turn. It is intentionally non-spinning: when the
/// ring is full it retains the pending fixed-size envelope and returns to the scheduler.
pub struct EngineOrchestrator {
    candidate_budget_per_turn: usize,
}

impl EngineOrchestrator {
    pub fn new(candidate_budget_per_turn: usize) -> Result<Self, RuntimeError> {
        if candidate_budget_per_turn == 0 {
            return Err(RuntimeError::Engine(EngineError::CandidateBudgetMustBePositive));
        }
        Ok(Self { candidate_budget_per_turn })
    }

    pub fn run_turn<const N: usize>(
        &self,
        engine: &AcquisitionEngine,
        frontier: &[FrontierNodeHot],
        cursor: &mut OrchestratorCursor,
        producer: &mut RingProducer<'_, N>,
    ) -> Result<RuntimeProgress, RuntimeError> {
        if let Some(event) = cursor.pending {
            match producer.try_enqueue(event) {
                Ok(()) => {
                    cursor.pending = None;
                    cursor.last_sent = cursor.pending_decision;
                    cursor.final_sent |= cursor.pending_final;
                    cursor.pending_decision = None;
                    cursor.pending_final = false;
                }
                Err(_) => return Ok(self.progress(cursor, true)),
            }
        }

        if cursor.final_sent {
            return Ok(self.progress(cursor, false));
        }

        let progress: SearchProgress = engine.step(
            frontier,
            &mut cursor.engine_cursor,
            self.candidate_budget_per_turn,
        )?;

        let should_emit = match progress.best {
            Some(best) if progress.complete => !cursor.final_sent,
            Some(best) => cursor.last_sent != Some(best),
            None => progress.complete,
        };

        if should_emit {
            if let Some(decision) = progress.best {
                let event = Self::to_envelope(frontier, decision, progress.complete)?;
                match producer.try_enqueue(event) {
                    Ok(()) => {
                        cursor.last_sent = Some(decision);
                        cursor.final_sent = progress.complete;
                    }
                    Err(returned) => {
                        cursor.pending = Some(returned);
                        cursor.pending_decision = Some(decision);
                        cursor.pending_final = progress.complete;
                        return Ok(self.progress(cursor, true));
                    }
                }
            } else if progress.complete {
                cursor.final_sent = true;
            }
        } else if progress.complete && progress.best.is_none() {
            cursor.final_sent = true;
        }

        Ok(self.progress(cursor, false))
    }

    fn to_envelope(
        frontier: &[FrontierNodeHot],
        decision: AcquisitionDecision,
        is_final: bool,
    ) -> Result<TaskEnvelope, RuntimeError> {
        let node = frontier.get(decision.candidate_index)
            .ok_or(RuntimeError::Engine(EngineError::CursorDoesNotMatchFrontier))?;
        let candidate_index = u32::try_from(decision.candidate_index)
            .map_err(|_| RuntimeError::CandidateIndexTooLarge(decision.candidate_index))?;
        let action_type = match decision.action {
            AcquisitionAction::EnrichMetadata => ACTION_ENRICH_METADATA,
            AcquisitionAction::ResolveCandidateRoute => ACTION_RESOLVE_ROUTE,
            AcquisitionAction::ValidateAuthorizedCandidate => ACTION_VALIDATE_CANDIDATE,
            AcquisitionAction::AcquireVerifiedAuthorizedPayload => ACTION_ACQUIRE_VERIFIED,
        };
        Ok(TaskEnvelope {
            paper_id: decision.id.paper_id,
            action_type,
            priority: decision.score.clamp(0, u16::MAX as i32) as u16,
            score: decision.score,
            source_id: node.source_id,
            policy_version: decision.id.policy_version,
            candidate_index,
            payload_hash: [0; 32],
            flags: if is_final { EVENT_FLAG_FINAL } else { 0 },
        })
    }

    fn progress<const N: usize>(
        &self,
        cursor: &OrchestratorCursor,
        backpressured: bool,
    ) -> RuntimeProgress {
        RuntimeProgress {
            candidates_evaluated: cursor.engine_cursor.candidates_evaluated(),
            total_candidates: cursor.engine_cursor_total(),
            completed: cursor.final_sent,
            backpressured,
            best: cursor.engine_cursor.best(),
        }
    }
}

impl OrchestratorCursor {
    fn engine_cursor_total(&self) -> usize {
        // Total count is not exposed on EngineCursor; it is invariantly stored at creation
        // and the caller provides a frontier of the same size to each run_turn.
        self.total_candidates
    }

    // Keep the total as an explicit runtime invariant without cloning the frontier.
    total_candidates: usize,
}
