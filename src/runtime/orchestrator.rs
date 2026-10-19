//! Connects the pure in-memory acquisition kernel to a bounded worker event queue.
//!
//! This module dispatches decisions only. It does not invent URLs, perform HTTP, authorize
//! sources, or declare download/indexing completion. Worker adapters must resolve actual
//! metadata and re-check source policy immediately before any request or download.

use crate::concurrency::ring::{
    RingProducer, TaskEnvelope, ACTION_ACQUIRE_VERIFIED, ACTION_ENRICH_METADATA,
    ACTION_RESOLVE_ROUTE, ACTION_VALIDATE_CANDIDATE, EVENT_FLAG_FINAL,
};
use crate::layers::layer2_state::FrontierNodeHot;
use crate::layers::layer5_control::{AcquisitionEngine, EngineCursor, EngineError};
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
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeProgress {
    pub candidates_evaluated: u64,
    pub total_candidates: usize,
    pub completed: bool,
    pub backpressured: bool,
    pub best: Option<AcquisitionDecision>,
}

/// Persistent loop state including a fixed-size event retained when the ring is full.
/// The caller must pass the same immutable frontier snapshot for this cursor's lifetime.
pub struct OrchestratorCursor {
    engine_cursor: EngineCursor,
    total_candidates: usize,
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
            total_candidates: frontier.len(),
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

/// One bounded event-loop turn. A full ring produces a resumable backpressure result instead
/// of an internal spin loop, and an unsent event is retained inside OrchestratorCursor.
pub struct EngineOrchestrator {
    candidate_budget_per_turn: usize,
}
impl EngineOrchestrator {
    pub fn new(candidate_budget_per_turn: usize) -> Result<Self, RuntimeError> {
        if candidate_budget_per_turn == 0 {
            return Err(RuntimeError::Engine(
                EngineError::CandidateBudgetMustBePositive,
            ));
        }
        Ok(Self {
            candidate_budget_per_turn,
        })
    }

    pub fn run_turn<const N: usize>(
        &self,
        engine: &AcquisitionEngine,
        frontier: &[FrontierNodeHot],
        cursor: &mut OrchestratorCursor,
        producer: &mut RingProducer<'_, N>,
    ) -> Result<RuntimeProgress, RuntimeError> {
        if cursor.total_candidates != frontier.len() {
            return Err(RuntimeError::Engine(
                EngineError::CursorDoesNotMatchFrontier,
            ));
        }

        if let Some(event) = cursor.pending {
            match producer.try_enqueue(event) {
                Ok(()) => {
                    cursor.pending = None;
                    cursor.last_sent = cursor.pending_decision;
                    cursor.final_sent |= cursor.pending_final;
                    cursor.pending_decision = None;
                    cursor.pending_final = false;
                }
                Err(_) => return Ok(Self::progress(cursor, true)),
            }
        }

        if cursor.final_sent {
            return Ok(Self::progress(cursor, false));
        }

        let progress = engine.step(
            frontier,
            &mut cursor.engine_cursor,
            self.candidate_budget_per_turn,
        )?;

        let should_emit = match progress.best {
            Some(_) if progress.complete => !cursor.final_sent,
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
                        return Ok(Self::progress(cursor, true));
                    }
                }
            } else if progress.complete {
                cursor.final_sent = true;
            }
        } else if progress.complete && progress.best.is_none() {
            cursor.final_sent = true;
        }

        Ok(Self::progress(cursor, false))
    }

    fn to_envelope(
        frontier: &[FrontierNodeHot],
        decision: AcquisitionDecision,
        is_final: bool,
    ) -> Result<TaskEnvelope, RuntimeError> {
        let node = frontier
            .get(decision.candidate_index)
            .ok_or(RuntimeError::Engine(
                EngineError::CursorDoesNotMatchFrontier,
            ))?;
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

    fn progress(cursor: &OrchestratorCursor, backpressured: bool) -> RuntimeProgress {
        RuntimeProgress {
            candidates_evaluated: cursor.engine_cursor.candidates_evaluated(),
            total_candidates: cursor.total_candidates,
            completed: cursor.final_sent,
            backpressured,
            best: cursor.engine_cursor.best(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concurrency::ring::{EngineRingBuffer, TaskEnvelope, EVENT_FLAG_FINAL};
    use crate::layers::layer2_state::{FLAG_HAS_METADATA, FLAG_RESTRICTED_BY_SOURCE};
    use crate::layers::layer5_control::PolicyConfig;

    fn make_engine() -> AcquisitionEngine {
        AcquisitionEngine::new(PolicyConfig::new(2026, 4).unwrap()).unwrap()
    }

    #[test]
    fn emits_final_decision_and_preserves_backpressure_without_spinning() {
        let frontier: Vec<_> = (1..=12)
            .map(|id| FrontierNodeHot {
                paper_id: id,
                citation_count: id as u32 * 20,
                source_id: 2,
                flags: FLAG_HAS_METADATA,
                ..FrontierNodeHot::default()
            })
            .collect();

        let engine = make_engine();
        let mut cursor = OrchestratorCursor::new(&engine, &frontier);
        let loop_engine = EngineOrchestrator::new(4).unwrap();
        let mut ring = EngineRingBuffer::<2>::new();
        let (mut producer, mut consumer) = ring.split();
        let mut observed_final = false;

        // Saturate the ring before dispatch. The orchestrator must retain the decision
        // instead of spinning or dropping it while capacity is unavailable.
        producer
            .try_enqueue(TaskEnvelope {
                paper_id: 900,
                ..TaskEnvelope::default()
            })
            .unwrap();
        producer
            .try_enqueue(TaskEnvelope {
                paper_id: 901,
                ..TaskEnvelope::default()
            })
            .unwrap();
        let blocked = loop_engine
            .run_turn(&engine, &frontier, &mut cursor, &mut producer)
            .unwrap();
        assert!(blocked.backpressured);
        assert!(!cursor.is_complete());
        assert_eq!(consumer.try_dequeue().unwrap().paper_id, 900);
        assert_eq!(consumer.try_dequeue().unwrap().paper_id, 901);

        for _ in 0..20 {
            let progress = loop_engine
                .run_turn(&engine, &frontier, &mut cursor, &mut producer)
                .unwrap();
            while let Some(event) = consumer.try_dequeue() {
                if event.flags & EVENT_FLAG_FINAL != 0 {
                    observed_final = true;
                    assert_eq!(event.paper_id, 12);
                }
            }
            if progress.completed {
                break;
            }
        }

        assert!(cursor.is_complete());
        assert!(observed_final);
    }

    #[test]
    fn restricted_high_score_candidate_is_never_dispatched() {
        let frontier = [
            FrontierNodeHot {
                paper_id: 1,
                citation_count: 1,
                flags: FLAG_HAS_METADATA,
                ..FrontierNodeHot::default()
            },
            FrontierNodeHot {
                paper_id: 2,
                citation_count: u32::MAX,
                flags: FLAG_HAS_METADATA | FLAG_RESTRICTED_BY_SOURCE,
                ..FrontierNodeHot::default()
            },
        ];
        let engine = make_engine();
        let mut cursor = OrchestratorCursor::new(&engine, &frontier);
        let loop_engine = EngineOrchestrator::new(8).unwrap();
        let mut ring = EngineRingBuffer::<4>::new();
        let (mut producer, mut consumer) = ring.split();
        loop {
            if loop_engine
                .run_turn(&engine, &frontier, &mut cursor, &mut producer)
                .unwrap()
                .completed
            {
                break;
            }
        }
        while let Some(event) = consumer.try_dequeue() {
            assert_eq!(event.paper_id, 1);
        }
    }
}
