//! Revision-bound live summary core.
//!
//! The core actor remains provider-neutral. `runtime` adds persistence and a
//! safe Tauri boundary without coupling summary work to the recording thread.

mod actor;
pub mod commands;
mod models;
mod openai_compatible;
mod recording_session;
mod runtime;

#[cfg(test)]
pub use actor::DeterministicFakeProvider;
pub use actor::{
    DispatchOutcome, LiveSummaryActor, LiveSummaryActorConfig, LiveSummaryActorError,
    LiveSummaryProvider, LiveSummaryProviderError, LiveSummaryProviderResponse,
    LiveSummaryProviderResult, LiveSummaryRecoveryState, LiveSummaryRequest,
    PreparedLiveSummaryRevision, StableSummarySource, SummaryResponseOutcome,
    SummaryResponsePreparation, SummarySourceChange, SummarySourceKind, SummarySubmitOutcome,
};
pub use models::{
    source_text_hash, LiveSummaryContractError, LiveSummaryItem, LiveSummaryRevision,
    LiveSummaryRevisionType, LiveSummaryScope, SummaryEvidence, SummaryItemKind, SummaryItemStatus,
};
pub(crate) use openai_compatible::{
    load_openai_compatible_config, LiveSummaryProviderEnvelope,
    OpenAiCompatibleLiveSummaryProviderFactory,
};
pub(crate) use recording_session::{
    install_recording_summary_ingress, mark_trusted_recording_pending,
    new_recording_summary_ingress, observe_trusted_transcript, RecordingSummaryIngress,
};
pub use recording_session::{
    RecordingLiveSummaryRegistry, RecordingLiveSummarySnapshot, RecordingSummaryBindingTicket,
    RecordingSummaryRegistryError, RecordingSummaryState,
};
pub use runtime::{
    LiveSummaryCoordinator, LiveSummaryCoordinatorError, LiveSummaryDispatchState,
    LiveSummaryEvent, LiveSummaryEventKind, LiveSummaryFrontendError, LiveSummaryLifecycle,
    LiveSummaryProviderAvailability, LiveSummaryProviderFactory, LiveSummaryPublicSnapshot,
    LiveSummaryRuntimeState, StartLiveSummarySession, UnavailableLiveSummaryProviderFactory,
    LIVE_SUMMARY_STATE_EVENT,
};
