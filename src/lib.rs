//! PaperEngine's pure in-memory acquisition-policy kernel.
//! All networking, filesystem access, clocks, and persistence belong in outer adapters.

pub mod concurrency;
pub mod engine_architecture;
pub mod layers;

pub use layers::layer1_input::{normalize_candidate, CandidateInput, InputError};
pub use layers::layer2_state::FrontierNodeHot;
pub use layers::layer5_control::{
    AcquisitionEngine, EngineCursor, EngineError, PolicyConfig, SearchProgress,
};
pub use layers::layer6_output::{
    AcquisitionAction, AcquisitionDecision, DecisionId, OutputError, OutputInterpreter,
};
