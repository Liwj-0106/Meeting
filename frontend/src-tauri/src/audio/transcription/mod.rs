// audio/transcription/mod.rs
//
// Transcription module: Provider abstraction, engine management, and worker pool.

pub mod engine;
pub mod event;
pub mod parakeet_provider;
pub mod provider;
pub mod streaming;
pub mod translation;
pub mod translation_runtime;
pub mod whisper_provider;
pub mod worker;

// Re-export commonly used types
pub use engine::{
    get_or_init_transcription_engine, get_or_init_whisper, validate_transcription_model_ready,
    TranscriptionEngine,
};
pub use event::{
    AsrMetadata, AudioSource, DiarizationMetadata, DiarizationStatus, ReplayApplyResult,
    SpeakerMetadata, SpeakerStatus, TranscriptChunkInput, TranscriptEvent, TranscriptEventKind,
    TranscriptNormalizationContext, TranscriptReplay, TranscriptUpdate,
    TRANSCRIPT_EVENT_SCHEMA_VERSION,
};
pub use parakeet_provider::ParakeetProvider;
pub use provider::{TranscriptResult, TranscriptionError, TranscriptionProvider};
pub use whisper_provider::WhisperProvider;
pub use worker::{reset_speech_detected_flag, start_transcription_task};
