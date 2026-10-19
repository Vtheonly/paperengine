use crate::layers::layer2_state::FrontierNodeHot;
use crate::layers::layer5_control::EngineError;

/// Formal engine lifecycle over pre-normalized in-memory state. No method performs I/O.
pub trait EngineArchitecture<State, Context, Output> {
    fn initialize_state(
        &self,
        frontier: &[FrontierNodeHot],
        context: &Context,
    ) -> Result<State, EngineError>;

    fn transition(
        &self,
        state: &mut State,
        frontier: &[FrontierNodeHot],
        context: &Context,
        candidate_budget: usize,
    ) -> Result<(), EngineError>;

    fn is_terminal(&self, state: &State, context: &Context) -> bool;

    fn project_result(
        &self,
        state: &State,
        frontier: &[FrontierNodeHot],
        context: &Context,
    ) -> Result<Output, EngineError>;
}
