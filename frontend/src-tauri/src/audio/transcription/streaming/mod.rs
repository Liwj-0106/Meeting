//! Pure core for streaming ASR providers.
//!
//! Stage 3 intentionally keeps transport, credentials, recording lifecycle,
//! and Tauri commands outside this module. These pieces define the testable
//! contract that those adapters consume.

pub mod deepgram;
pub mod health;
pub mod normalizer;
pub mod openai_realtime;
pub mod openai_supervisor;
pub mod pcm;
pub mod protocol;
pub mod ring_buffer;
pub mod supervisor;

pub use deepgram::{
    build_deepgram_url, encode_deepgram_control, parse_deepgram_message, DeepgramAlternative,
    DeepgramConfigError, DeepgramConnection, DeepgramControl, DeepgramError, DeepgramEventDecoder,
    DeepgramFailureStage, DeepgramMessage, DeepgramMetadata, DeepgramModelInfo,
    DeepgramNetworkErrorKind, DeepgramOptions, DeepgramParseError, DeepgramProviderError,
    DeepgramResultMetadata, DeepgramResults, DeepgramServerEvent, DeepgramSpeechStarted,
    DeepgramTranscript, DeepgramUtteranceEnd, DEEPGRAM_CHANNELS, DEEPGRAM_LIVE_ENDPOINT,
    DEEPGRAM_SAMPLE_RATE,
};
pub use health::{
    AsrHealthApplyResult, AsrHealthEvent, AsrHealthEventKind, AsrHealthReducer, AsrHealthSeverity,
    AsrHealthSnapshot, AsrHealthState,
};
pub use normalizer::{
    NormalizeOutcome, NormalizerError, StreamingTranscriptContext, StreamingTranscriptNormalizer,
};
pub use openai_realtime::{
    build_openai_realtime_url, encode_openai_audio_append, encode_openai_control,
    encode_openai_session_update, parse_openai_realtime_message, OpenAiAudioBufferCleared,
    OpenAiAudioBufferCommitted, OpenAiErrorEvent, OpenAiProviderError, OpenAiRateLimit,
    OpenAiRateLimitsUpdated, OpenAiRealtimeConfigError, OpenAiRealtimeConnection,
    OpenAiRealtimeControl, OpenAiRealtimeDelay, OpenAiRealtimeError, OpenAiRealtimeEventDecoder,
    OpenAiRealtimeFailureStage, OpenAiRealtimeMessage, OpenAiRealtimeNetworkErrorKind,
    OpenAiRealtimeOptions, OpenAiRealtimeParseError, OpenAiRealtimeServerEvent,
    OpenAiRealtimeTurnDetection, OpenAiSessionLifecycle, OpenAiSpeechStarted, OpenAiSpeechStopped,
    OpenAiTranscriptCompleted, OpenAiTranscriptCompletedUpdate, OpenAiTranscriptDelta,
    OpenAiTranscriptDeltaUpdate, OpenAiTranscriptFailed, OpenAiTranscriptFailedUpdate,
    OPENAI_REALTIME_CHANNELS, OPENAI_REALTIME_ENDPOINT, OPENAI_REALTIME_SAMPLE_RATE,
};
pub use openai_supervisor::{
    run_openai_supervisor, OpenAiSupervisorConfig, OpenAiSupervisorExit, OpenAiSupervisorOutput,
};
pub use pcm::{f32_to_pcm16, pcm16_le_bytes, PcmError, PersistentPcmResampler, ProviderSampleRate};
pub use protocol::{
    ProviderTimeRange, ProviderTranscriptEvent, ProviderTranscriptKind, ProviderTranscriptWord,
    StreamingAsrCapabilities, StreamingAsrCommand, StreamingAsrProvider, StreamingAudioEncoding,
    StreamingAudioFrame, StreamingFlushReason, StreamingProtocolError,
    STREAMING_ASR_SCHEMA_VERSION, STREAMING_AUDIO_CHANNELS, STREAMING_AUDIO_SAMPLE_RATE,
};
pub use ring_buffer::{
    ReplayAudioWindow, RingBufferError, RingWriteReport, StreamingAudioRingBuffer,
    DEFAULT_REPLAY_PREROLL_FRAMES, DEFAULT_REPLAY_PREROLL_MS, DEFAULT_RING_FRAMES,
    DEFAULT_RING_SECONDS,
};
pub use supervisor::{
    run_deepgram_supervisor, DeepgramSupervisorConfig, DeepgramSupervisorExit,
    DeepgramSupervisorOutput, SharedAsrHealthState,
};
