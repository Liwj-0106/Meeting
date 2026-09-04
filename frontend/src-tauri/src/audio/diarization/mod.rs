//! Provider-neutral speaker-diarization primitives.
//!
//! The protocol and supervisor remain independent from a concrete model
//! implementation. The JSONL process transport connects their bounded control
//! surface to an explicitly configured local Python worker.

pub mod moss_protocol;
pub mod process_transport;
pub mod session_runtime;
pub mod speaker_stabilizer;
pub mod worker_supervisor;

pub use moss_protocol::{
    MossProtocolError, MossProtocolLimits, MossSegment, MossWorkerRequest, MossWorkerResponse,
    MOSS_WORKER_SCHEMA_VERSION,
};
pub use process_transport::{
    MossJsonlProcessFactory, MossJsonlProcessTransport, MossProcessConfigError,
    MossProcessTransportConfig,
};
pub use session_runtime::{
    CanonicalTranscriptEmitter, CanonicalTranscriptSink, DiarizationIngressError,
    DiarizationIngressErrorCode, DiarizationRuntimeAvailability, DiarizationRuntimeStartError,
    DiarizationRuntimeStatus, DiarizationSessionConfig, DiarizationSessionIngress,
    DiarizationSessionRuntime,
};
pub use speaker_stabilizer::{
    stabilize_speakers, SpeakerAssignment, SpeakerStabilizerConfig, StabilizationResult,
    StabilizedObservation, StableSpeakerProfile,
};
pub use worker_supervisor::{
    MossTransportError, MossTransportErrorCode, MossWorkerBackoffPolicy, MossWorkerError,
    MossWorkerErrorCode, MossWorkerHandshake, MossWorkerJob, MossWorkerState, MossWorkerStatus,
    MossWorkerSupervisor, MossWorkerSupervisorConfig, MossWorkerTransport,
    MossWorkerTransportFactory,
};
