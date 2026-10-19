//! Last-chance validation before an in-memory decision reaches an outer adapter.

use core::fmt;
use super::layer2_state::{
    FrontierNodeHot, FLAG_AUTHORIZED_FULL_TEXT, FLAG_HAS_CANDIDATE_URL, FLAG_HAS_METADATA,
    FLAG_PDF_SIGNATURE_VERIFIED, FLAG_RESTRICTED_BY_SOURCE,
};
use super::layer4_optimization::score_candidates_batch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquisitionAction {
    EnrichMetadata,
    ResolveCandidateRoute,
    ValidateAuthorizedCandidate,
    AcquireVerifiedAuthorizedPayload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DecisionId {
    pub paper_id: u64,
    pub policy_version: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcquisitionDecision {
    pub id: DecisionId,
    pub candidate_index: usize,
    pub score: i32,
    pub action: AcquisitionAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputError {
    CandidateIndexOutOfBounds,
    CandidateIdentityMismatch,
    RestrictedCandidate,
    PolicyVersionMismatch,
    ActionInvariantViolation,
    ScoreMismatch,
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "acquisition output invariant failed: {self:?}")
    }
}
impl std::error::Error for OutputError {}

pub(crate) fn action_for_candidate(node: &FrontierNodeHot) -> AcquisitionAction {
    if !node.has_flag(FLAG_HAS_METADATA) {
        AcquisitionAction::EnrichMetadata
    } else if !node.has_flag(FLAG_HAS_CANDIDATE_URL)
        || !node.has_flag(FLAG_AUTHORIZED_FULL_TEXT)
    {
        AcquisitionAction::ResolveCandidateRoute
    } else if !node.has_flag(FLAG_PDF_SIGNATURE_VERIFIED) {
        AcquisitionAction::ValidateAuthorizedCandidate
    } else {
        AcquisitionAction::AcquireVerifiedAuthorizedPayload
    }
}

pub struct OutputInterpreter;

impl OutputInterpreter {
    pub fn validate_projection(
        frontier: &[FrontierNodeHot],
        decision: AcquisitionDecision,
        reference_year: u16,
        policy_version: u32,
    ) -> Result<AcquisitionDecision, OutputError> {
        let node = frontier.get(decision.candidate_index)
            .ok_or(OutputError::CandidateIndexOutOfBounds)?;
        if node.paper_id != decision.id.paper_id {
            return Err(OutputError::CandidateIdentityMismatch);
        }
        if decision.id.policy_version != policy_version {
            return Err(OutputError::PolicyVersionMismatch);
        }
        if node.has_flag(FLAG_RESTRICTED_BY_SOURCE) {
            return Err(OutputError::RestrictedCandidate);
        }
        if action_for_candidate(node) != decision.action {
            return Err(OutputError::ActionInvariantViolation);
        }
        let score = score_candidates_batch(core::slice::from_ref(node), reference_year, false)[0];
        if score != decision.score { return Err(OutputError::ScoreMismatch); }
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::layer2_state::{
        FLAG_AUTHORIZED_FULL_TEXT, FLAG_HAS_CANDIDATE_URL, FLAG_HAS_METADATA,
        FLAG_PDF_SIGNATURE_VERIFIED, FLAG_RESTRICTED_BY_SOURCE,
    };

    #[test]
    fn restricted_candidate_rejected_at_output_boundary() {
        let node = FrontierNodeHot {
            paper_id: 7,
            flags: FLAG_HAS_METADATA | FLAG_HAS_CANDIDATE_URL | FLAG_AUTHORIZED_FULL_TEXT
                | FLAG_PDF_SIGNATURE_VERIFIED | FLAG_RESTRICTED_BY_SOURCE,
            ..FrontierNodeHot::default()
        };
        let decision = AcquisitionDecision {
            id: DecisionId { paper_id: 7, policy_version: 1 }, candidate_index: 0,
            score: 0, action: AcquisitionAction::AcquireVerifiedAuthorizedPayload,
        };
        assert_eq!(
            OutputInterpreter::validate_projection(&[node], decision, 2026, 1),
            Err(OutputError::RestrictedCandidate)
        );
    }

    #[test]
    fn action_requires_authorization_and_verified_pdf() {
        let node = FrontierNodeHot {
            paper_id: 7,
            flags: FLAG_HAS_METADATA | FLAG_HAS_CANDIDATE_URL | FLAG_AUTHORIZED_FULL_TEXT,
            ..FrontierNodeHot::default()
        };
        let score = score_candidates_batch(core::slice::from_ref(&node), 2026, false)[0];
        let decision = AcquisitionDecision {
            id: DecisionId { paper_id: 7, policy_version: 2 }, candidate_index: 0,
            score, action: AcquisitionAction::AcquireVerifiedAuthorizedPayload,
        };
        assert_eq!(
            OutputInterpreter::validate_projection(&[node], decision, 2026, 2),
            Err(OutputError::ActionInvariantViolation)
        );
    }
}
