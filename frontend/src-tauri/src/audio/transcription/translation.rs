//! Revision-bound, provider-neutral live text translation primitives.
//!
//! This module deliberately contains no network client and no Tauri state. It
//! defines the source/result contract plus a deterministic in-memory
//! coordinator that can be tested without audio, credentials, or a runtime.
//! A future provider adapter owns I/O and feeds complete translation snapshots
//! back through [`TranslationCoordinator::handle_response`]. Token deltas are
//! never part of the durable contract.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::hash::{Hash, Hasher};
use thiserror::Error;
use uuid::Uuid;

pub const TRANSLATION_EVENT_SCHEMA_VERSION: u16 = 1;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_LANGUAGE_TAG_BYTES: usize = 35;
const MAX_SOURCE_TEXT_CHARS: usize = 65_536;
const MAX_TRANSLATED_TEXT_CHARS: usize = 65_536;

/// The immutable transcript version a translation was generated from.
///
/// `event_id`, `revision`, and `text_hash` form the binding gate. Meeting and
/// utterance IDs provide storage scope but never replace that three-part gate.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationSourceBinding {
    pub meeting_id: String,
    pub event_id: String,
    pub utterance_id: String,
    pub revision: u64,
    /// Lowercase SHA-256 hex of the exact normalized source text.
    pub text_hash: String,
}

impl fmt::Debug for TranslationSourceBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationSourceBinding")
            .field("meeting_id", &self.meeting_id)
            .field("event_id", &self.event_id)
            .field("utterance_id", &self.utterance_id)
            .field("revision", &self.revision)
            .field("text_hash", &"<sha256>")
            .finish()
    }
}

impl TranslationSourceBinding {
    pub fn same_source_version(&self, other: &Self) -> bool {
        self.meeting_id == other.meeting_id
            && self.utterance_id == other.utterance_id
            && self.event_id == other.event_id
            && self.revision == other.revision
            && self.text_hash == other.text_hash
    }

    fn validate(&self) -> Result<(), TranslationContractError> {
        validate_identifier("meeting_id", &self.meeting_id)?;
        validate_identifier("event_id", &self.event_id)?;
        validate_identifier("utterance_id", &self.utterance_id)?;
        validate_text_hash(&self.text_hash)
    }
}

/// Immutable glossary content used by one translation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlossaryVersionBinding {
    pub glossary_id: String,
    pub version: u64,
    pub content_hash: String,
}

impl GlossaryVersionBinding {
    fn validate(&self) -> Result<(), TranslationContractError> {
        validate_identifier("glossary_id", &self.glossary_id)?;
        if self.version == 0 {
            return Err(TranslationContractError::InvalidField {
                field: "glossary.version",
                reason: "must be at least 1",
            });
        }
        validate_hash("glossary.content_hash", &self.content_hash)
    }
}

/// Inputs which can change a translation without changing the transcript
/// source triple. The request identity is `(TranslationSourceBinding,
/// TranslationRequestFingerprint)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationRequestFingerprint {
    pub source_language: String,
    pub target_language: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glossary: Option<GlossaryVersionBinding>,
}

impl TranslationRequestFingerprint {
    fn validate(&self) -> Result<(), TranslationContractError> {
        validate_language_tag("fingerprint.source_language", &self.source_language)?;
        validate_language_tag("fingerprint.target_language", &self.target_language)?;
        validate_bounded_identifier("fingerprint.provider", &self.provider, 128)?;
        if let Some(model) = self.model.as_deref() {
            validate_identifier("fingerprint.model", model)?;
        }
        if let Some(glossary) = self.glossary.as_ref() {
            glossary.validate()?;
        }
        if self
            .source_language
            .eq_ignore_ascii_case(&self.target_language)
        {
            return Err(TranslationContractError::InvalidField {
                field: "fingerprint.target_language",
                reason: "must differ from source_language",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationSourceKind {
    Partial,
    Final,
    Correction,
    SpeakerUpdate,
    LanguageUpdate,
    Retraction,
}

impl TranslationSourceKind {
    fn priority(self) -> u8 {
        match self {
            Self::Partial => 1,
            Self::Final => 2,
            Self::Correction => 3,
            Self::SpeakerUpdate | Self::LanguageUpdate => 4,
            Self::Retraction => 5,
        }
    }

    fn is_final_priority(self) -> bool {
        !matches!(self, Self::Partial | Self::Retraction)
    }
}

/// Complete source snapshot submitted to the coordinator.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationSourceEvent {
    pub source: TranslationSourceBinding,
    pub event_kind: TranslationSourceKind,
    pub is_stable: bool,
    pub source_text: String,
    pub source_language: String,
    pub target_language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glossary: Option<GlossaryVersionBinding>,
}

impl fmt::Debug for TranslationSourceEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationSourceEvent")
            .field("source", &self.source)
            .field("event_kind", &self.event_kind)
            .field("is_stable", &self.is_stable)
            .field("source_text_chars", &self.source_text.chars().count())
            .field("source_language", &self.source_language)
            .field("target_language", &self.target_language)
            .field("speaker_id", &self.speaker_id)
            .field("glossary", &self.glossary)
            .finish()
    }
}

impl TranslationSourceEvent {
    pub fn validate(&self) -> Result<(), TranslationContractError> {
        self.source.validate()?;
        validate_language_tag("source_language", &self.source_language)?;
        validate_language_tag("target_language", &self.target_language)?;
        if self
            .source_language
            .eq_ignore_ascii_case(&self.target_language)
        {
            return Err(TranslationContractError::InvalidField {
                field: "target_language",
                reason: "must differ from source_language",
            });
        }
        if let Some(speaker_id) = self.speaker_id.as_deref() {
            validate_identifier("speaker_id", speaker_id)?;
        }
        if let Some(glossary) = self.glossary.as_ref() {
            glossary.validate()?;
        }

        let source_chars = self.source_text.chars().count();
        if source_chars > MAX_SOURCE_TEXT_CHARS {
            return Err(TranslationContractError::InvalidField {
                field: "source_text",
                reason: "exceeds the complete-snapshot character limit",
            });
        }
        if self.source_text.contains('\0') {
            return Err(TranslationContractError::InvalidField {
                field: "source_text",
                reason: "contains a NUL character",
            });
        }
        if self.source.text_hash != source_text_hash(&self.source_text) {
            return Err(TranslationContractError::InvalidField {
                field: "text_hash",
                reason: "must equal SHA-256(source_text)",
            });
        }
        if self.event_kind != TranslationSourceKind::Retraction
            && self.source_text.trim().is_empty()
        {
            return Err(TranslationContractError::InvalidField {
                field: "source_text",
                reason: "must not be empty",
            });
        }
        if self.event_kind == TranslationSourceKind::Partial && self.is_stable {
            return Err(TranslationContractError::InvalidField {
                field: "is_stable",
                reason: "a partial source cannot be stable",
            });
        }
        if self.event_kind != TranslationSourceKind::Partial && !self.is_stable {
            return Err(TranslationContractError::InvalidField {
                field: "is_stable",
                reason: "a final, correction, metadata update, or retraction must be stable",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationEventKind {
    Snapshot,
    Retraction,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationStatus {
    Partial,
    Final,
    Reused,
    Retracted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationErrorCode {
    ProviderRejected,
    ProviderFailed,
    InvalidResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationError {
    pub code: TranslationErrorCode,
    /// Sanitized provider-independent message, capped before persistence.
    pub message: String,
    pub retryable: bool,
}

/// Full translation snapshot bound to exactly one transcript version.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationEvent {
    pub schema_version: u16,
    pub translation_event_id: String,
    pub source: TranslationSourceBinding,
    pub source_kind: TranslationSourceKind,
    pub translation_revision: u64,
    pub generation: u64,
    pub event_kind: TranslationEventKind,
    pub status: TranslationStatus,
    pub request_fingerprint: TranslationRequestFingerprint,
    pub source_language: String,
    pub target_language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translated_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glossary: Option<GlossaryVersionBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_from_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TranslationError>,
    /// Monotonic coordinator time. A persistence adapter may add wall-clock time.
    pub created_at_ms: u64,
}

impl fmt::Debug for TranslationEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationEvent")
            .field("translation_event_id", &self.translation_event_id)
            .field("source", &self.source)
            .field("source_kind", &self.source_kind)
            .field("translation_revision", &self.translation_revision)
            .field("generation", &self.generation)
            .field("event_kind", &self.event_kind)
            .field("status", &self.status)
            .field("request_fingerprint", &self.request_fingerprint)
            .field(
                "translated_text_chars",
                &self
                    .translated_text
                    .as_ref()
                    .map(|text| text.chars().count()),
            )
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("reused_from_event_id", &self.reused_from_event_id)
            .field("latency_ms", &self.latency_ms)
            .field("error_code", &self.error.as_ref().map(|error| error.code))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationRequest {
    pub request_id: String,
    pub generation: u64,
    pub source: TranslationSourceBinding,
    pub source_kind: TranslationSourceKind,
    pub source_text: String,
    pub fingerprint: TranslationRequestFingerprint,
    pub is_final: bool,
}

impl fmt::Debug for TranslationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationRequest")
            .field("request_id", &self.request_id)
            .field("generation", &self.generation)
            .field("source", &self.source)
            .field("source_kind", &self.source_kind)
            .field("source_text_chars", &self.source_text.chars().count())
            .field("fingerprint", &self.fingerprint)
            .field("is_final", &self.is_final)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct TranslationProviderError {
    pub code: TranslationErrorCode,
    pub retryable: bool,
}

impl fmt::Debug for TranslationProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationProviderError")
            .field("code", &self.code)
            .field("retryable", &self.retryable)
            .finish()
    }
}

pub trait TranslationProvider {
    fn provider_id(&self) -> &str;
    fn model_id(&self) -> Option<&str>;
    fn start(&mut self, request: &TranslationRequest) -> Result<(), TranslationProviderError>;
    fn cancel(&mut self, request_id: &str);
}

#[derive(Clone, PartialEq, Eq)]
pub enum TranslationProviderResult {
    Success {
        translated_text: String,
        latency_ms: u64,
    },
    Failure(TranslationProviderError),
}

impl fmt::Debug for TranslationProviderResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success {
                translated_text,
                latency_ms,
            } => formatter
                .debug_struct("Success")
                .field("translated_text_chars", &translated_text.chars().count())
                .field("latency_ms", latency_ms)
                .finish(),
            Self::Failure(error) => formatter.debug_tuple("Failure").field(error).finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct TranslationProviderResponse {
    pub request_id: String,
    pub generation: u64,
    pub source: TranslationSourceBinding,
    pub fingerprint: TranslationRequestFingerprint,
    pub completed_at_ms: u64,
    pub result: TranslationProviderResult,
}

impl fmt::Debug for TranslationProviderResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationProviderResponse")
            .field("request_id", &self.request_id)
            .field("generation", &self.generation)
            .field("source", &self.source)
            .field("fingerprint", &self.fingerprint)
            .field("completed_at_ms", &self.completed_at_ms)
            .field("result", &self.result)
            .finish()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TranslationContractError {
    #[error("invalid translation field {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("translation queue capacity must be at least one")]
    InvalidQueueCapacity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationCoordinatorConfig {
    /// Maximum number of waiting requests, including debounced partials.
    /// At most one separately tracked provider request may be in flight.
    pub queue_capacity: usize,
    pub partial_debounce_ms: u64,
}

impl Default for TranslationCoordinatorConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 32,
            partial_debounce_ms: 350,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationIngestDecision {
    Debounced {
        generation: u64,
        coalesced: bool,
    },
    QueuedFinal {
        generation: u64,
    },
    Reused(TranslationEvent),
    Retracted(TranslationEvent),
    DuplicateSource,
    StaleSource,
    DroppedPartial,
    /// Ownership is returned so the durable transcript consumer can retry.
    /// A final source is never silently discarded by the bounded queue.
    RetryFinal(TranslationSourceEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationDispatchDecision {
    Idle,
    Busy,
    Started { request_id: String, generation: u64 },
    ProviderRejected(TranslationEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationResponseDecision {
    Applied(TranslationEvent),
    Failed(TranslationEvent),
    IgnoredLate,
}

/// Deterministic no-network provider used by contract tests and dry runs.
#[derive(Clone, Default)]
pub struct DeterministicFakeTranslationProvider {
    provider_id: String,
    model_id: Option<String>,
    started: Vec<TranslationRequest>,
    canceled: Vec<String>,
    fail_next_start: Option<TranslationProviderError>,
}

impl DeterministicFakeTranslationProvider {
    pub fn new(provider_id: impl Into<String>, model_id: Option<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id,
            ..Self::default()
        }
    }

    pub fn started_requests(&self) -> &[TranslationRequest] {
        &self.started
    }

    pub fn canceled_request_ids(&self) -> &[String] {
        &self.canceled
    }

    pub fn fail_next_start(&mut self, error: TranslationProviderError) {
        self.fail_next_start = Some(error);
    }

    pub fn success_response(
        &self,
        request_index: usize,
        translated_text: impl Into<String>,
        latency_ms: u64,
        completed_at_ms: u64,
    ) -> TranslationProviderResponse {
        let request = &self.started[request_index];
        TranslationProviderResponse {
            request_id: request.request_id.clone(),
            generation: request.generation,
            source: request.source.clone(),
            fingerprint: request.fingerprint.clone(),
            completed_at_ms,
            result: TranslationProviderResult::Success {
                translated_text: translated_text.into(),
                latency_ms,
            },
        }
    }
}

impl TranslationProvider for DeterministicFakeTranslationProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn model_id(&self) -> Option<&str> {
        self.model_id.as_deref()
    }

    fn start(&mut self, request: &TranslationRequest) -> Result<(), TranslationProviderError> {
        if let Some(error) = self.fail_next_start.take() {
            return Err(error);
        }
        self.started.push(request.clone());
        Ok(())
    }

    fn cancel(&mut self, request_id: &str) {
        self.canceled.push(request_id.to_string());
    }
}

#[derive(Clone, Eq)]
struct TranslationKey {
    meeting_id: String,
    utterance_id: String,
    target_language: String,
}

impl TranslationKey {
    fn from_source(source: &TranslationSourceEvent) -> Self {
        Self {
            meeting_id: source.source.meeting_id.clone(),
            utterance_id: source.source.utterance_id.clone(),
            target_language: source.target_language.to_ascii_lowercase(),
        }
    }
}

impl PartialEq for TranslationKey {
    fn eq(&self, other: &Self) -> bool {
        self.meeting_id == other.meeting_id
            && self.utterance_id == other.utterance_id
            && self.target_language == other.target_language
    }
}

impl Hash for TranslationKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.meeting_id.hash(state);
        self.utterance_id.hash(state);
        self.target_language.hash(state);
    }
}

#[derive(Clone)]
struct PendingPartial {
    source: TranslationSourceEvent,
    fingerprint: TranslationRequestFingerprint,
    generation: u64,
    due_at_ms: u64,
    order: u64,
}

#[derive(Clone)]
struct QueuedTranslation {
    request: TranslationRequest,
    source: TranslationSourceEvent,
    order: u64,
}

struct InFlightTranslation {
    request: TranslationRequest,
    source: TranslationSourceEvent,
}

#[derive(Clone)]
struct CompletedTranslation {
    event: TranslationEvent,
    fingerprint: TranslationRequestFingerprint,
}

/// Pure state machine for low-latency bilingual text captions.
///
/// The queue is bounded. Partial work may be coalesced or evicted; a final
/// that cannot fit is returned to the caller as `RetryFinal`, preserving
/// explicit backpressure rather than silently dropping durable transcript data.
pub struct TranslationCoordinator<P: TranslationProvider> {
    provider: P,
    config: TranslationCoordinatorConfig,
    generations: HashMap<TranslationKey, u64>,
    latest_sources: HashMap<TranslationKey, TranslationSourceEvent>,
    latest_fingerprints: HashMap<TranslationKey, TranslationRequestFingerprint>,
    translation_revisions: HashMap<TranslationKey, u64>,
    pending_partials: HashMap<TranslationKey, PendingPartial>,
    ready: VecDeque<QueuedTranslation>,
    in_flight: Option<InFlightTranslation>,
    completed: HashMap<TranslationKey, CompletedTranslation>,
    emitted: VecDeque<TranslationEvent>,
    order_sequence: u64,
}

impl<P: TranslationProvider> TranslationCoordinator<P> {
    pub fn new(
        provider: P,
        config: TranslationCoordinatorConfig,
    ) -> Result<Self, TranslationContractError> {
        if config.queue_capacity == 0 {
            return Err(TranslationContractError::InvalidQueueCapacity);
        }
        Ok(Self {
            provider,
            config,
            generations: HashMap::new(),
            latest_sources: HashMap::new(),
            latest_fingerprints: HashMap::new(),
            translation_revisions: HashMap::new(),
            pending_partials: HashMap::new(),
            ready: VecDeque::new(),
            in_flight: None,
            completed: HashMap::new(),
            emitted: VecDeque::new(),
            order_sequence: 0,
        })
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    pub fn provider_mut(&mut self) -> &mut P {
        &mut self.provider
    }

    pub fn waiting_len(&self) -> usize {
        self.pending_partials.len() + self.ready.len()
    }

    pub fn has_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    pub fn take_events(&mut self) -> Vec<TranslationEvent> {
        self.emitted.drain(..).collect()
    }

    /// Restore durable state before accepting live work after restart. Older
    /// history can raise the global counter high-water marks, but it can never
    /// replace a newer source head or that head's request fingerprint.
    pub fn restore_latest_source(
        &mut self,
        source: TranslationSourceEvent,
        request_fingerprint: TranslationRequestFingerprint,
        last_generation: u64,
        last_translation_revision: u64,
    ) -> Result<(), TranslationContractError> {
        source.validate()?;
        request_fingerprint.validate()?;
        if !fingerprint_matches_source(&request_fingerprint, &source) {
            return Err(TranslationContractError::InvalidField {
                field: "recovery_request_fingerprint",
                reason: "languages and glossary must match the restored source",
            });
        }
        if last_generation == 0 || last_translation_revision == 0 {
            return Err(TranslationContractError::InvalidField {
                field: "recovery_counters",
                reason: "latest persisted translation counters must be at least 1",
            });
        }
        let key = TranslationKey::from_source(&source);
        let current_generation = self.generations.get(&key).copied().unwrap_or(0);
        let should_restore_head = match self.latest_sources.get(&key) {
            None => true,
            Some(current) => match source.source.revision.cmp(&current.source.revision) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    source.event_kind.priority() > current.event_kind.priority()
                        || (source.source.same_source_version(&current.source)
                            && last_generation > current_generation)
                }
            },
        };
        let generation = self.generations.entry(key.clone()).or_insert(0);
        *generation = (*generation).max(last_generation);
        let revision = self.translation_revisions.entry(key.clone()).or_insert(0);
        *revision = (*revision).max(last_translation_revision);
        if should_restore_head {
            self.latest_sources.insert(key.clone(), source);
            self.latest_fingerprints.insert(key, request_fingerprint);
        }
        Ok(())
    }

    pub fn ingest(
        &mut self,
        source: TranslationSourceEvent,
        now_ms: u64,
    ) -> Result<TranslationIngestDecision, TranslationContractError> {
        source.validate()?;
        let key = TranslationKey::from_source(&source);
        let fingerprint = self.request_fingerprint(&source)?;

        if let Some(latest) = self.latest_sources.get(&key) {
            if source.source.same_source_version(&latest.source) {
                if self.latest_fingerprints.get(&key) == Some(&fingerprint) {
                    return Ok(TranslationIngestDecision::DuplicateSource);
                }
            } else {
                match compare_sources(&source, latest) {
                    SourceOrder::Duplicate => {
                        return Ok(TranslationIngestDecision::DuplicateSource)
                    }
                    SourceOrder::Stale | SourceOrder::Conflict => {
                        return Ok(TranslationIngestDecision::StaleSource)
                    }
                    SourceOrder::Newer => {}
                }
            }
        }

        if source.event_kind == TranslationSourceKind::Retraction {
            self.invalidate_key(&key);
            let generation = self.bump_generation(&key);
            self.latest_sources.insert(key.clone(), source.clone());
            self.latest_fingerprints
                .insert(key.clone(), fingerprint.clone());
            self.completed.remove(&key);
            let event = self.make_terminal_event(
                &key,
                &source,
                &fingerprint,
                generation,
                TranslationEventKind::Retraction,
                TranslationStatus::Retracted,
                None,
                None,
                None,
                None,
                None,
                now_ms,
            );
            self.emitted.push_back(event.clone());
            return Ok(TranslationIngestDecision::Retracted(event));
        }

        if source.event_kind == TranslationSourceKind::SpeakerUpdate {
            if let Some(previous) = self.completed.get(&key).cloned() {
                let reusable = previous.event.source.text_hash == source.source.text_hash
                    && previous.fingerprint == fingerprint
                    && matches!(
                        previous.event.status,
                        TranslationStatus::Final | TranslationStatus::Reused
                    );
                if reusable {
                    self.invalidate_key(&key);
                    let generation = self.bump_generation(&key);
                    self.latest_sources.insert(key.clone(), source.clone());
                    self.latest_fingerprints
                        .insert(key.clone(), fingerprint.clone());
                    let event = self.make_terminal_event(
                        &key,
                        &source,
                        &fingerprint,
                        generation,
                        TranslationEventKind::Snapshot,
                        TranslationStatus::Reused,
                        previous.event.translated_text.clone(),
                        previous.event.provider.clone(),
                        previous.event.model.clone(),
                        Some(previous.event.translation_event_id.clone()),
                        previous.event.latency_ms,
                        now_ms,
                    );
                    self.completed.insert(
                        key,
                        CompletedTranslation {
                            event: event.clone(),
                            fingerprint,
                        },
                    );
                    self.emitted.push_back(event.clone());
                    return Ok(TranslationIngestDecision::Reused(event));
                }
            }
        }

        if source.event_kind == TranslationSourceKind::Partial {
            let coalesced = self.contains_waiting_key(&key)
                || self
                    .in_flight
                    .as_ref()
                    .map(|item| TranslationKey::from_source(&item.source) == key)
                    .unwrap_or(false);

            if !coalesced && self.waiting_len() >= self.config.queue_capacity {
                if !self.evict_oldest_partial_except(None) {
                    return Ok(TranslationIngestDecision::DroppedPartial);
                }
            }

            self.invalidate_key(&key);
            let generation = self.bump_generation(&key);
            self.latest_sources.insert(key.clone(), source.clone());
            self.latest_fingerprints
                .insert(key.clone(), fingerprint.clone());
            self.completed.remove(&key);
            let order = self.next_order();
            self.pending_partials.insert(
                key,
                PendingPartial {
                    source,
                    fingerprint,
                    generation,
                    due_at_ms: now_ms.saturating_add(self.config.partial_debounce_ms),
                    order,
                },
            );
            return Ok(TranslationIngestDecision::Debounced {
                generation,
                coalesced,
            });
        }

        // Compute the final's ability to fit before mutating state. If every
        // waiting slot holds another final, return ownership for an explicit
        // retry; this is the lossless side of the bounded-queue contract.
        let waiting_without_key = self.waiting_count_except(&key);
        if waiting_without_key >= self.config.queue_capacity
            && !self.has_waiting_partial_except(Some(&key))
        {
            return Ok(TranslationIngestDecision::RetryFinal(source));
        }

        self.invalidate_key(&key);
        while self.waiting_len() >= self.config.queue_capacity {
            if !self.evict_oldest_partial_except(Some(&key)) {
                // Defensive branch: the preflight above should make this
                // unreachable, but returning ownership remains lossless.
                return Ok(TranslationIngestDecision::RetryFinal(source));
            }
        }
        self.preempt_unrelated_partial();
        let generation = self.bump_generation(&key);
        self.latest_sources.insert(key.clone(), source.clone());
        self.latest_fingerprints
            .insert(key.clone(), fingerprint.clone());
        self.completed.remove(&key);
        let request = self.make_request(&source, generation, fingerprint);
        let order = self.next_order();
        let queued = QueuedTranslation {
            request,
            source,
            order,
        };
        let final_insert_at = self
            .ready
            .iter()
            .position(|item| !item.request.is_final)
            .unwrap_or(self.ready.len());
        self.ready.insert(final_insert_at, queued);
        Ok(TranslationIngestDecision::QueuedFinal { generation })
    }

    /// Move partial snapshots whose debounce deadline has elapsed to the
    /// provider-ready queue. The waiting-item count does not change.
    pub fn advance(&mut self, now_ms: u64) {
        let mut due: Vec<(TranslationKey, PendingPartial)> = self
            .pending_partials
            .iter()
            .filter(|(_, item)| item.due_at_ms <= now_ms)
            .map(|(key, item)| (key.clone(), item.clone()))
            .collect();
        due.sort_by_key(|(_, item)| item.order);
        for (key, item) in due {
            self.pending_partials.remove(&key);
            let request = self.make_request(&item.source, item.generation, item.fingerprint);
            self.ready.push_back(QueuedTranslation {
                request,
                source: item.source,
                order: item.order,
            });
        }
    }

    pub fn dispatch_next(&mut self, now_ms: u64) -> TranslationDispatchDecision {
        self.advance(now_ms);
        if self.in_flight.is_some() {
            return TranslationDispatchDecision::Busy;
        }

        while let Some(item) = self.ready.pop_front() {
            let key = TranslationKey::from_source(&item.source);
            let generation_is_current =
                self.generations.get(&key).copied() == Some(item.request.generation);
            let binding_is_current = self
                .latest_sources
                .get(&key)
                .map(|latest| latest.source.same_source_version(&item.request.source))
                .unwrap_or(false);
            let fingerprint_is_current =
                self.latest_fingerprints.get(&key) == Some(&item.request.fingerprint);
            if !generation_is_current || !binding_is_current || !fingerprint_is_current {
                continue;
            }

            if let Err(error) = self.provider.start(&item.request) {
                let event = self.make_terminal_event(
                    &key,
                    &item.source,
                    &item.request.fingerprint,
                    item.request.generation,
                    TranslationEventKind::Error,
                    TranslationStatus::Failed,
                    None,
                    Some(item.request.fingerprint.provider.clone()),
                    item.request.fingerprint.model.clone(),
                    None,
                    None,
                    now_ms,
                );
                let event = TranslationEvent {
                    error: Some(sanitize_provider_error(error)),
                    ..event
                };
                self.emitted.push_back(event.clone());
                return TranslationDispatchDecision::ProviderRejected(event);
            }

            let request_id = item.request.request_id.clone();
            let generation = item.request.generation;
            self.in_flight = Some(InFlightTranslation {
                request: item.request,
                source: item.source,
            });
            return TranslationDispatchDecision::Started {
                request_id,
                generation,
            };
        }
        TranslationDispatchDecision::Idle
    }

    pub fn handle_response(
        &mut self,
        response: TranslationProviderResponse,
    ) -> TranslationResponseDecision {
        let Some(current) = self.in_flight.as_ref() else {
            return TranslationResponseDecision::IgnoredLate;
        };
        if current.request.request_id != response.request_id {
            return TranslationResponseDecision::IgnoredLate;
        }

        let current = self.in_flight.take().expect("checked in-flight request");
        let key = TranslationKey::from_source(&current.source);
        let generation_is_current =
            self.generations.get(&key).copied() == Some(current.request.generation);
        let latest_is_current = self
            .latest_sources
            .get(&key)
            .map(|latest| latest.source.same_source_version(&current.request.source))
            .unwrap_or(false);
        let fingerprint_is_current =
            self.latest_fingerprints.get(&key) == Some(&current.request.fingerprint);
        if !generation_is_current || !latest_is_current || !fingerprint_is_current {
            return TranslationResponseDecision::IgnoredLate;
        }

        let response_binding_valid = response.generation == current.request.generation
            && response.source.same_source_version(&current.request.source)
            && response.fingerprint == current.request.fingerprint;
        if !response_binding_valid {
            let event = self.failed_response_event(
                &key,
                &current,
                TranslationError {
                    code: TranslationErrorCode::InvalidResponse,
                    message: "provider response did not match request binding".to_string(),
                    retryable: false,
                },
                response.completed_at_ms,
            );
            return TranslationResponseDecision::Failed(event);
        }

        match response.result {
            TranslationProviderResult::Failure(error) => {
                let event = self.failed_response_event(
                    &key,
                    &current,
                    sanitize_provider_error(error),
                    response.completed_at_ms,
                );
                TranslationResponseDecision::Failed(event)
            }
            TranslationProviderResult::Success {
                translated_text,
                latency_ms,
            } => {
                if translated_text.trim().is_empty()
                    || translated_text.chars().count() > MAX_TRANSLATED_TEXT_CHARS
                    || translated_text.contains('\0')
                {
                    let event = self.failed_response_event(
                        &key,
                        &current,
                        TranslationError {
                            code: TranslationErrorCode::InvalidResponse,
                            message: "provider returned an invalid complete snapshot".to_string(),
                            retryable: false,
                        },
                        response.completed_at_ms,
                    );
                    return TranslationResponseDecision::Failed(event);
                }

                let status = if current.request.is_final {
                    TranslationStatus::Final
                } else {
                    TranslationStatus::Partial
                };
                let event = self.make_terminal_event(
                    &key,
                    &current.source,
                    &current.request.fingerprint,
                    current.request.generation,
                    TranslationEventKind::Snapshot,
                    status,
                    Some(translated_text),
                    Some(current.request.fingerprint.provider.clone()),
                    current.request.fingerprint.model.clone(),
                    None,
                    Some(latency_ms),
                    response.completed_at_ms,
                );
                if status == TranslationStatus::Final {
                    self.completed.insert(
                        key,
                        CompletedTranslation {
                            event: event.clone(),
                            fingerprint: current.request.fingerprint.clone(),
                        },
                    );
                }
                self.emitted.push_back(event.clone());
                TranslationResponseDecision::Applied(event)
            }
        }
    }

    fn failed_response_event(
        &mut self,
        key: &TranslationKey,
        current: &InFlightTranslation,
        error: TranslationError,
        now_ms: u64,
    ) -> TranslationEvent {
        let event = self.make_terminal_event(
            key,
            &current.source,
            &current.request.fingerprint,
            current.request.generation,
            TranslationEventKind::Error,
            TranslationStatus::Failed,
            None,
            Some(current.request.fingerprint.provider.clone()),
            current.request.fingerprint.model.clone(),
            None,
            None,
            now_ms,
        );
        let event = TranslationEvent {
            error: Some(error),
            ..event
        };
        self.emitted.push_back(event.clone());
        event
    }

    #[allow(clippy::too_many_arguments)]
    fn make_terminal_event(
        &mut self,
        key: &TranslationKey,
        source: &TranslationSourceEvent,
        request_fingerprint: &TranslationRequestFingerprint,
        generation: u64,
        event_kind: TranslationEventKind,
        status: TranslationStatus,
        translated_text: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        reused_from_event_id: Option<String>,
        latency_ms: Option<u64>,
        now_ms: u64,
    ) -> TranslationEvent {
        let translation_revision = self.next_translation_revision(key);
        let translation_event_id = next_id("translation-event");
        TranslationEvent {
            schema_version: TRANSLATION_EVENT_SCHEMA_VERSION,
            translation_event_id,
            source: source.source.clone(),
            source_kind: source.event_kind,
            translation_revision,
            generation,
            event_kind,
            status,
            request_fingerprint: request_fingerprint.clone(),
            source_language: request_fingerprint.source_language.clone(),
            target_language: request_fingerprint.target_language.clone(),
            translated_text,
            provider,
            model,
            glossary: request_fingerprint.glossary.clone(),
            speaker_id: source.speaker_id.clone(),
            reused_from_event_id,
            latency_ms,
            error: None,
            created_at_ms: now_ms,
        }
    }

    fn make_request(
        &mut self,
        source: &TranslationSourceEvent,
        generation: u64,
        fingerprint: TranslationRequestFingerprint,
    ) -> TranslationRequest {
        TranslationRequest {
            request_id: next_id("translation-request"),
            generation,
            source: source.source.clone(),
            source_kind: source.event_kind,
            source_text: source.source_text.clone(),
            fingerprint,
            is_final: source.event_kind.is_final_priority(),
        }
    }

    fn request_fingerprint(
        &self,
        source: &TranslationSourceEvent,
    ) -> Result<TranslationRequestFingerprint, TranslationContractError> {
        let fingerprint = TranslationRequestFingerprint {
            source_language: source.source_language.to_ascii_lowercase(),
            target_language: source.target_language.to_ascii_lowercase(),
            provider: self.provider.provider_id().to_string(),
            model: self.provider.model_id().map(str::to_string),
            glossary: source.glossary.clone(),
        };
        fingerprint.validate()?;
        Ok(fingerprint)
    }

    fn next_order(&mut self) -> u64 {
        self.order_sequence = self.order_sequence.saturating_add(1);
        self.order_sequence
    }

    fn bump_generation(&mut self, key: &TranslationKey) -> u64 {
        let generation = self.generations.entry(key.clone()).or_insert(0);
        *generation = generation.saturating_add(1);
        *generation
    }

    fn next_translation_revision(&mut self, key: &TranslationKey) -> u64 {
        let revision = self.translation_revisions.entry(key.clone()).or_insert(0);
        *revision = revision.saturating_add(1);
        *revision
    }

    fn contains_waiting_key(&self, key: &TranslationKey) -> bool {
        self.pending_partials.contains_key(key)
            || self
                .ready
                .iter()
                .any(|item| TranslationKey::from_source(&item.source) == *key)
    }

    fn waiting_count_except(&self, key: &TranslationKey) -> usize {
        self.pending_partials
            .keys()
            .filter(|candidate| *candidate != key)
            .count()
            + self
                .ready
                .iter()
                .filter(|item| TranslationKey::from_source(&item.source) != *key)
                .count()
    }

    fn has_waiting_partial_except(&self, excluded: Option<&TranslationKey>) -> bool {
        self.pending_partials
            .keys()
            .any(|key| excluded.map(|excluded| excluded != key).unwrap_or(true))
            || self.ready.iter().any(|item| {
                !item.request.is_final
                    && excluded
                        .map(|excluded| TranslationKey::from_source(&item.source) != *excluded)
                        .unwrap_or(true)
            })
    }

    fn evict_oldest_partial_except(&mut self, excluded: Option<&TranslationKey>) -> bool {
        let pending_candidate = self
            .pending_partials
            .iter()
            .filter(|(key, _)| excluded.map(|excluded| excluded != *key).unwrap_or(true))
            .min_by_key(|(_, item)| item.order)
            .map(|(key, item)| (key.clone(), item.order));
        let ready_candidate = self
            .ready
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                !item.request.is_final
                    && excluded
                        .map(|excluded| TranslationKey::from_source(&item.source) != *excluded)
                        .unwrap_or(true)
            })
            .min_by_key(|(_, item)| item.order)
            .map(|(index, item)| (index, item.order));

        match (pending_candidate, ready_candidate) {
            (None, None) => false,
            (Some((key, _)), None) => {
                self.pending_partials.remove(&key);
                true
            }
            (None, Some((index, _))) => {
                self.ready.remove(index);
                true
            }
            (Some((key, pending_order)), Some((index, ready_order))) => {
                if pending_order <= ready_order {
                    self.pending_partials.remove(&key);
                } else {
                    self.ready.remove(index);
                }
                true
            }
        }
    }

    fn invalidate_key(&mut self, key: &TranslationKey) {
        self.pending_partials.remove(key);
        self.ready
            .retain(|item| TranslationKey::from_source(&item.source) != *key);
        let matches_in_flight = self
            .in_flight
            .as_ref()
            .map(|item| TranslationKey::from_source(&item.source) == *key)
            .unwrap_or(false);
        if matches_in_flight {
            if let Some(item) = self.in_flight.take() {
                self.provider.cancel(&item.request.request_id);
            }
        }
    }

    fn preempt_unrelated_partial(&mut self) {
        let should_preempt = self
            .in_flight
            .as_ref()
            .map(|item| !item.request.is_final)
            .unwrap_or(false);
        if !should_preempt {
            return;
        }
        if let Some(item) = self.in_flight.take() {
            self.provider.cancel(&item.request.request_id);
            let key = TranslationKey::from_source(&item.source);
            self.bump_generation(&key);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceOrder {
    Newer,
    Duplicate,
    Stale,
    Conflict,
}

fn compare_sources(
    incoming: &TranslationSourceEvent,
    current: &TranslationSourceEvent,
) -> SourceOrder {
    if incoming.source.same_source_version(&current.source) {
        return SourceOrder::Duplicate;
    }
    match incoming.source.revision.cmp(&current.source.revision) {
        std::cmp::Ordering::Greater => SourceOrder::Newer,
        std::cmp::Ordering::Less => SourceOrder::Stale,
        std::cmp::Ordering::Equal => {
            if incoming.event_kind.priority() > current.event_kind.priority() {
                SourceOrder::Newer
            } else {
                SourceOrder::Conflict
            }
        }
    }
}

fn fingerprint_matches_source(
    fingerprint: &TranslationRequestFingerprint,
    source: &TranslationSourceEvent,
) -> bool {
    fingerprint
        .source_language
        .eq_ignore_ascii_case(&source.source_language)
        && fingerprint
            .target_language
            .eq_ignore_ascii_case(&source.target_language)
        && fingerprint.glossary == source.glossary
}

fn sanitize_provider_error(error: TranslationProviderError) -> TranslationError {
    let message = match error.code {
        TranslationErrorCode::ProviderRejected => "translation provider rejected the request",
        TranslationErrorCode::ProviderFailed => "translation provider request failed",
        TranslationErrorCode::InvalidResponse => {
            "translation provider returned an invalid response"
        }
    }
    .to_string();
    TranslationError {
        code: error.code,
        message,
        retryable: error.retryable,
    }
}

fn next_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), TranslationContractError> {
    validate_bounded_identifier(field, value, MAX_IDENTIFIER_BYTES)
}

fn validate_bounded_identifier(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), TranslationContractError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(TranslationContractError::InvalidField {
            field,
            reason: "must contain a bounded, non-empty UTF-8 identifier",
        });
    }
    if value.chars().any(char::is_control) {
        return Err(TranslationContractError::InvalidField {
            field,
            reason: "must not contain control characters",
        });
    }
    Ok(())
}

fn validate_language_tag(field: &'static str, value: &str) -> Result<(), TranslationContractError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_LANGUAGE_TAG_BYTES
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return Err(TranslationContractError::InvalidField {
            field,
            reason: "must be a compact BCP-47-style language tag",
        });
    }
    Ok(())
}

fn validate_text_hash(value: &str) -> Result<(), TranslationContractError> {
    validate_hash("text_hash", value)
}

fn validate_hash(field: &'static str, value: &str) -> Result<(), TranslationContractError> {
    if value.len() != 64
        || !value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(TranslationContractError::InvalidField {
            field,
            reason: "must be 64 lowercase hexadecimal characters",
        });
    }
    Ok(())
}

/// SHA-256 of the exact source snapshot bytes. Normalization, if any, belongs
/// upstream and must happen before both this call and provider dispatch.
pub fn source_text_hash(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let padded_len = (bytes.len() + 9).div_ceil(64) * 64;
    let mut padded = Vec::with_capacity(padded_len);
    padded.extend_from_slice(bytes);
    padded.push(0x80);
    padded.resize(padded_len - 8, 0);
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for block in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut output = String::with_capacity(64);
    for word in state {
        use std::fmt::Write;
        write!(&mut output, "{word:08x}").expect("write SHA-256 hex into String");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn source(
        utterance: &str,
        event: &str,
        revision: u64,
        _text_hash: &str,
        text: &str,
        kind: TranslationSourceKind,
    ) -> TranslationSourceEvent {
        TranslationSourceEvent {
            source: TranslationSourceBinding {
                meeting_id: "meeting-1".to_string(),
                event_id: event.to_string(),
                utterance_id: utterance.to_string(),
                revision,
                text_hash: source_text_hash(text),
            },
            event_kind: kind,
            is_stable: kind != TranslationSourceKind::Partial,
            source_text: text.to_string(),
            source_language: "ja".to_string(),
            target_language: "zh-CN".to_string(),
            speaker_id: Some("speaker-1".to_string()),
            glossary: None,
        }
    }

    fn coordinator(
        capacity: usize,
        debounce_ms: u64,
    ) -> TranslationCoordinator<DeterministicFakeTranslationProvider> {
        TranslationCoordinator::new(
            DeterministicFakeTranslationProvider::new(
                "deterministic-fake",
                Some("fixture-model".to_string()),
            ),
            TranslationCoordinatorConfig {
                queue_capacity: capacity,
                partial_debounce_ms: debounce_ms,
            },
        )
        .expect("valid coordinator")
    }

    fn fake_fingerprint() -> TranslationRequestFingerprint {
        TranslationRequestFingerprint {
            source_language: "ja".to_string(),
            target_language: "zh-cn".to_string(),
            provider: "deterministic-fake".to_string(),
            model: Some("fixture-model".to_string()),
            glossary: None,
        }
    }

    #[test]
    fn source_contract_requires_exact_lowercase_sha256_binding() {
        let valid = source(
            "utterance-1",
            "event-1",
            0,
            HASH_A,
            "こんにちは",
            TranslationSourceKind::Final,
        );
        assert!(valid.validate().is_ok());

        assert_eq!(
            source_text_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            source_text_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let mut wrong_content = valid.clone();
        wrong_content.source.text_hash = HASH_A.to_string();
        assert!(matches!(
            wrong_content.validate(),
            Err(TranslationContractError::InvalidField {
                field: "text_hash",
                ..
            })
        ));

        let mut uppercase = valid.clone();
        uppercase.source.text_hash = valid.source.text_hash.to_ascii_uppercase();
        assert!(matches!(
            uppercase.validate(),
            Err(TranslationContractError::InvalidField {
                field: "text_hash",
                ..
            })
        ));
    }

    #[test]
    fn partials_are_debounced_and_coalesced_to_the_newest_binding() {
        let mut coordinator = coordinator(4, 300);
        let first = source(
            "utterance-1",
            "event-partial-1",
            0,
            HASH_A,
            "こん",
            TranslationSourceKind::Partial,
        );
        let second = source(
            "utterance-1",
            "event-partial-2",
            1,
            HASH_B,
            "こんにちは",
            TranslationSourceKind::Partial,
        );

        assert!(matches!(
            coordinator.ingest(first, 100).expect("first partial"),
            TranslationIngestDecision::Debounced {
                coalesced: false,
                ..
            }
        ));
        assert!(matches!(
            coordinator.ingest(second, 250).expect("second partial"),
            TranslationIngestDecision::Debounced {
                coalesced: true,
                ..
            }
        ));
        assert_eq!(
            coordinator.dispatch_next(549),
            TranslationDispatchDecision::Idle
        );
        assert!(matches!(
            coordinator.dispatch_next(550),
            TranslationDispatchDecision::Started { generation: 2, .. }
        ));
        assert_eq!(coordinator.provider().started_requests().len(), 1);
        assert_eq!(
            coordinator.provider().started_requests()[0].source.event_id,
            "event-partial-2"
        );
    }

    #[test]
    fn final_preempts_partial_and_late_canceled_response_is_generation_gated() {
        let mut coordinator = coordinator(4, 0);
        let partial = source(
            "utterance-1",
            "event-partial",
            0,
            HASH_A,
            "こん",
            TranslationSourceKind::Partial,
        );
        coordinator.ingest(partial, 0).expect("queue partial");
        assert!(matches!(
            coordinator.dispatch_next(0),
            TranslationDispatchDecision::Started { generation: 1, .. }
        ));
        let late = coordinator
            .provider()
            .success_response(0, "迟到译文", 50, 50);

        let final_source = source(
            "utterance-1",
            "event-final",
            1,
            HASH_B,
            "こんにちは",
            TranslationSourceKind::Final,
        );
        assert!(matches!(
            coordinator.ingest(final_source, 10).expect("queue final"),
            TranslationIngestDecision::QueuedFinal { generation: 2 }
        ));
        assert_eq!(
            coordinator.provider().canceled_request_ids().len(),
            1,
            "provider cancellation is advisory; generation remains authoritative"
        );
        assert_eq!(
            coordinator.handle_response(late),
            TranslationResponseDecision::IgnoredLate
        );

        assert!(matches!(
            coordinator.dispatch_next(10),
            TranslationDispatchDecision::Started { generation: 2, .. }
        ));
        let accepted = coordinator.provider().success_response(1, "你好", 80, 90);
        let applied = coordinator.handle_response(accepted);
        assert!(matches!(
            applied,
            TranslationResponseDecision::Applied(TranslationEvent {
                status: TranslationStatus::Final,
                generation: 2,
                ..
            })
        ));
    }

    #[test]
    fn retraction_cancels_work_and_emits_a_bound_tombstone() {
        let mut coordinator = coordinator(4, 0);
        let final_source = source(
            "utterance-1",
            "event-final",
            1,
            HASH_A,
            "こんにちは",
            TranslationSourceKind::Final,
        );
        coordinator.ingest(final_source, 0).expect("queue final");
        coordinator.dispatch_next(0);
        let late = coordinator.provider().success_response(0, "你好", 20, 20);

        let retraction = source(
            "utterance-1",
            "event-retraction",
            2,
            HASH_B,
            "",
            TranslationSourceKind::Retraction,
        );
        let decision = coordinator.ingest(retraction, 10).expect("retract");
        assert!(matches!(
            decision,
            TranslationIngestDecision::Retracted(TranslationEvent {
                event_kind: TranslationEventKind::Retraction,
                status: TranslationStatus::Retracted,
                translated_text: None,
                ..
            })
        ));
        assert_eq!(
            coordinator.handle_response(late),
            TranslationResponseDecision::IgnoredLate
        );
    }

    #[test]
    fn speaker_only_update_reuses_a_final_with_the_same_text_hash() {
        let mut coordinator = coordinator(4, 0);
        let final_source = source(
            "utterance-1",
            "event-final",
            1,
            HASH_A,
            "こんにちは",
            TranslationSourceKind::Final,
        );
        coordinator.ingest(final_source, 0).expect("queue final");
        coordinator.dispatch_next(0);
        let response = coordinator.provider().success_response(0, "你好", 20, 20);
        coordinator.handle_response(response);

        let mut speaker_update = source(
            "utterance-1",
            "event-speaker",
            2,
            HASH_A,
            "こんにちは",
            TranslationSourceKind::SpeakerUpdate,
        );
        speaker_update.speaker_id = Some("speaker-2".to_string());
        let reused = coordinator.ingest(speaker_update, 30).expect("reuse");
        assert!(matches!(
            reused,
            TranslationIngestDecision::Reused(TranslationEvent {
                status: TranslationStatus::Reused,
                translated_text: Some(ref text),
                ..
            }) if text == "你好"
        ));
        assert_eq!(
            coordinator.provider().started_requests().len(),
            1,
            "speaker metadata changes do not call the provider again"
        );
    }

    #[test]
    fn bounded_queue_never_silently_drops_a_final() {
        let mut coordinator = coordinator(2, 500);
        let first = source(
            "utterance-1",
            "event-1",
            1,
            HASH_A,
            "一",
            TranslationSourceKind::Final,
        );
        let second = source(
            "utterance-2",
            "event-2",
            1,
            HASH_A,
            "二",
            TranslationSourceKind::Final,
        );
        let third = source(
            "utterance-3",
            "event-3",
            1,
            HASH_A,
            "三",
            TranslationSourceKind::Final,
        );
        coordinator.ingest(first, 0).expect("first final");
        coordinator.ingest(second, 0).expect("second final");

        let retry = match coordinator.ingest(third, 0).expect("bounded decision") {
            TranslationIngestDecision::RetryFinal(source) => source,
            other => panic!("expected lossless retry, got {other:?}"),
        };
        assert_eq!(coordinator.waiting_len(), 2);
        coordinator.dispatch_next(0);
        assert!(matches!(
            coordinator
                .ingest(retry, 1)
                .expect("retry after one dispatch"),
            TranslationIngestDecision::QueuedFinal { .. }
        ));
        assert_eq!(coordinator.waiting_len(), 2);
    }

    #[test]
    fn restored_counters_never_restart_generation_or_translation_revision() {
        let mut coordinator = coordinator(2, 0);
        let restored = source(
            "utterance-1",
            "event-restored",
            4,
            HASH_A,
            "こんにちは",
            TranslationSourceKind::Final,
        );
        coordinator
            .restore_latest_source(restored.clone(), fake_fingerprint(), 10, 20)
            .expect("restore durable source head and counters");
        let delayed_older_history = source(
            "utterance-1",
            "event-before-restart",
            3,
            HASH_A,
            "こんにち",
            TranslationSourceKind::Final,
        );
        coordinator
            .restore_latest_source(delayed_older_history.clone(), fake_fingerprint(), 11, 21)
            .expect("older history raises counters without replacing the source head");

        assert!(matches!(
            coordinator
                .ingest(delayed_older_history, 0)
                .expect("reject replay older than restored head"),
            TranslationIngestDecision::StaleSource
        ));
        assert_eq!(coordinator.waiting_len(), 0);

        assert!(matches!(
            coordinator
                .ingest(
                    source(
                        "utterance-1",
                        "event-after-restart",
                        5,
                        HASH_A,
                        "こんにちは",
                        TranslationSourceKind::Final,
                    ),
                    0,
                )
                .expect("queue restored source"),
            TranslationIngestDecision::QueuedFinal { generation: 12 }
        ));
        coordinator.dispatch_next(0);
        let response = coordinator.provider().success_response(0, "你好", 10, 10);
        assert!(matches!(
            coordinator.handle_response(response),
            TranslationResponseDecision::Applied(TranslationEvent {
                translation_revision: 22,
                ..
            })
        ));
    }

    #[test]
    fn recovery_preserves_old_request_fingerprint_so_provider_changes_retranslate() {
        let mut coordinator = coordinator(2, 0);
        let restored = source(
            "utterance-1",
            "event-restored",
            4,
            HASH_A,
            "こんにちは",
            TranslationSourceKind::Final,
        );
        let mut previous_fingerprint = fake_fingerprint();
        previous_fingerprint.provider = "previous-provider".to_string();
        previous_fingerprint.model = Some("previous-model".to_string());
        coordinator
            .restore_latest_source(restored.clone(), previous_fingerprint, 10, 20)
            .expect("restore old provider identity");

        assert!(matches!(
            coordinator
                .ingest(restored, 0)
                .expect("same source must retranslate after input fingerprint changes"),
            TranslationIngestDecision::QueuedFinal { generation: 11 }
        ));
    }

    #[test]
    fn glossary_version_changes_request_fingerprint_and_cancels_old_generation() {
        let mut coordinator = coordinator(2, 0);
        let mut first = source(
            "utterance-1",
            "event-final",
            1,
            HASH_A,
            "こんにちは",
            TranslationSourceKind::Final,
        );
        first.glossary = Some(GlossaryVersionBinding {
            glossary_id: "meeting-terms".to_string(),
            version: 1,
            content_hash: HASH_A.to_string(),
        });
        coordinator.ingest(first.clone(), 0).expect("queue v1");
        coordinator.dispatch_next(0);
        let late_v1 = coordinator.provider().success_response(0, "旧术语", 10, 10);

        let mut second = first;
        second.glossary = Some(GlossaryVersionBinding {
            glossary_id: "meeting-terms".to_string(),
            version: 2,
            content_hash: HASH_B.to_string(),
        });
        assert!(matches!(
            coordinator.ingest(second, 1).expect("queue v2"),
            TranslationIngestDecision::QueuedFinal { generation: 2 }
        ));
        assert_eq!(coordinator.provider().canceled_request_ids().len(), 1);
        assert_eq!(
            coordinator.handle_response(late_v1),
            TranslationResponseDecision::IgnoredLate
        );

        coordinator.dispatch_next(1);
        let latest_request = &coordinator.provider().started_requests()[1];
        assert_eq!(
            latest_request
                .fingerprint
                .glossary
                .as_ref()
                .unwrap()
                .version,
            2
        );

        let mut tampered = coordinator.provider().success_response(1, "新术语", 10, 20);
        tampered.fingerprint.provider = "another-provider".to_string();
        assert!(matches!(
            coordinator.handle_response(tampered),
            TranslationResponseDecision::Failed(TranslationEvent {
                error: Some(TranslationError {
                    code: TranslationErrorCode::InvalidResponse,
                    ..
                }),
                ..
            })
        ));
    }

    #[test]
    fn final_evicts_waiting_partial_and_is_dispatched_first() {
        let mut coordinator = coordinator(2, 100);
        coordinator
            .ingest(
                source(
                    "utterance-p1",
                    "partial-1",
                    0,
                    HASH_A,
                    "一",
                    TranslationSourceKind::Partial,
                ),
                0,
            )
            .expect("partial one");
        coordinator
            .ingest(
                source(
                    "utterance-p2",
                    "partial-2",
                    0,
                    HASH_A,
                    "二",
                    TranslationSourceKind::Partial,
                ),
                0,
            )
            .expect("partial two");
        coordinator.advance(100);
        coordinator
            .ingest(
                source(
                    "utterance-final",
                    "final-1",
                    1,
                    HASH_B,
                    "三",
                    TranslationSourceKind::Final,
                ),
                100,
            )
            .expect("final replaces partial capacity");
        coordinator.dispatch_next(100);
        assert_eq!(
            coordinator.provider().started_requests()[0].source.event_id,
            "final-1"
        );
    }

    #[test]
    fn response_must_match_event_revision_and_text_hash_together() {
        let mut coordinator = coordinator(2, 0);
        coordinator
            .ingest(
                source(
                    "utterance-1",
                    "event-1",
                    1,
                    HASH_A,
                    "こんにちは",
                    TranslationSourceKind::Final,
                ),
                0,
            )
            .expect("queue final");
        coordinator.dispatch_next(0);
        let mut response = coordinator.provider().success_response(0, "你好", 10, 10);
        response.source.text_hash = HASH_B.to_string();
        assert!(matches!(
            coordinator.handle_response(response),
            TranslationResponseDecision::Failed(TranslationEvent {
                error: Some(TranslationError {
                    code: TranslationErrorCode::InvalidResponse,
                    ..
                }),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn migration_projects_only_the_newest_complete_snapshot() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("create in-memory database");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable SQLite foreign keys");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");

        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind("meeting-translation")
            .bind("Synthetic translation test")
            .bind("2026-09-02T00:00:00Z")
            .bind("2026-09-02T00:00:00Z")
            .execute(&pool)
            .await
            .expect("insert meeting");
        sqlx::query(
            r#"INSERT INTO utterance_revisions (
                event_id, meeting_id, schema_version, utterance_id, revision,
                event_kind, is_stable, transcript, timestamp, created_at
            ) VALUES (?, ?, 1, ?, 1, 'final', 1, ?, ?, ?)"#,
        )
        .bind("source-event-1")
        .bind("meeting-translation")
        .bind("utterance-1")
        .bind("fixture source")
        .bind("00:00:01")
        .bind("2026-09-02T00:00:01Z")
        .execute(&pool)
        .await
        .expect("insert source revision");

        let insert_translation =
            |event_id: &str, translation_revision: i64, generation: i64, translated_text: &str| {
                sqlx::query(
                    r#"INSERT INTO translation_revisions (
                    translation_event_id, meeting_id, utterance_id,
                    source_event_id, source_revision, source_kind, source_text_hash,
                    translation_revision, generation, event_kind, status,
                    source_language, target_language, translated_text,
                    provider, created_at
                ) VALUES (?, ?, ?, ?, 1, 'final', ?, ?, ?, 'snapshot', 'final',
                          'ja', 'zh-CN', ?, 'fixture-provider', ?)"#,
                )
                .bind(event_id.to_string())
                .bind("meeting-translation")
                .bind("utterance-1")
                .bind("source-event-1")
                .bind(HASH_A)
                .bind(translation_revision)
                .bind(generation)
                .bind(translated_text.to_string())
                .bind("2026-09-02T00:00:02Z")
            };

        insert_translation("translation-event-3", 3, 3, "最新译文")
            .execute(&pool)
            .await
            .expect("insert newest translation first");
        insert_translation("translation-event-2", 2, 2, "迟到旧译文")
            .execute(&pool)
            .await
            .expect("retain delayed history row");

        let latest: (String, i64) = sqlx::query_as(
            "SELECT translated_text, translation_revision FROM translation_latest WHERE meeting_id = ? AND utterance_id = ? AND target_language = ?",
        )
        .bind("meeting-translation")
        .bind("utterance-1")
        .bind("zh-CN")
        .fetch_one(&pool)
        .await
        .expect("read latest projection");
        assert_eq!(latest, ("最新译文".to_string(), 3));

        sqlx::query(
            r#"INSERT INTO utterance_revisions (
                event_id, meeting_id, schema_version, utterance_id, revision,
                event_kind, is_stable, transcript, timestamp, created_at
            ) VALUES (?, ?, 1, ?, 2, 'speaker_update', 1, ?, ?, ?)"#,
        )
        .bind("source-speaker-2")
        .bind("meeting-translation")
        .bind("utterance-1")
        .bind("fixture source")
        .bind("00:00:01")
        .bind("2026-09-02T00:00:03Z")
        .execute(&pool)
        .await
        .expect("insert speaker-only source revision");
        sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, translated_text,
                provider, reused_from_event_id, created_at
            ) VALUES (?, ?, ?, ?, 2, 'speaker_update', ?, 4, 4, 'snapshot', 'reused',
                      'ja', 'zh-cn', ?, 'fixture-provider', ?, ?)"#,
        )
        .bind("translation-event-4")
        .bind("meeting-translation")
        .bind("utterance-1")
        .bind("source-speaker-2")
        .bind(HASH_A)
        .bind("最新译文")
        .bind("translation-event-3")
        .bind("2026-09-02T00:00:04Z")
        .execute(&pool)
        .await
        .expect("insert scoped speaker-only reuse");

        let reused_latest: (String, i64, String) = sqlx::query_as(
            "SELECT translated_text, translation_revision, status FROM translation_latest WHERE meeting_id = ? AND utterance_id = ? AND target_language = ?",
        )
        .bind("meeting-translation")
        .bind("utterance-1")
        .bind("ZH-CN")
        .fetch_one(&pool)
        .await
        .expect("read case-insensitive reused projection");
        assert_eq!(
            reused_latest,
            ("最新译文".to_string(), 4, "reused".to_string())
        );

        for (event_id, event_kind, is_stable, transcript) in [
            ("source-priority-partial", "partial", 0_i64, "part"),
            ("source-priority-final", "final", 1_i64, "complete"),
        ] {
            sqlx::query(
                r#"INSERT INTO utterance_revisions (
                    event_id, meeting_id, schema_version, utterance_id, revision,
                    event_kind, is_stable, transcript, timestamp, created_at
                ) VALUES (?, ?, 1, ?, 0, ?, ?, ?, ?, ?)"#,
            )
            .bind(event_id)
            .bind("meeting-translation")
            .bind("utterance-source-priority")
            .bind(event_kind)
            .bind(is_stable)
            .bind(transcript)
            .bind("00:00:01")
            .bind("2026-09-02T00:00:05Z")
            .execute(&pool)
            .await
            .expect("insert same-revision source priority event");
        }

        for (
            translation_event_id,
            source_event_id,
            source_kind,
            source_hash,
            counter,
            status,
            translated_text,
        ) in [
            (
                "translation-priority-partial",
                "source-priority-partial",
                "partial",
                HASH_A,
                1_i64,
                "partial",
                "part translated",
            ),
            (
                "translation-priority-final",
                "source-priority-final",
                "final",
                HASH_B,
                2_i64,
                "final",
                "complete translated",
            ),
            (
                "translation-priority-partial-late",
                "source-priority-partial",
                "partial",
                HASH_A,
                3_i64,
                "partial",
                "late partial translated",
            ),
        ] {
            sqlx::query(
                r#"INSERT INTO translation_revisions (
                    translation_event_id, meeting_id, utterance_id,
                    source_event_id, source_revision, source_kind, source_text_hash,
                    translation_revision, generation, event_kind, status,
                    source_language, target_language, translated_text,
                    provider, created_at
                ) VALUES (?, ?, ?, ?, 0, ?, ?, ?, ?, 'snapshot', ?,
                          'ja', 'zh-CN', ?, 'fixture-provider', ?)"#,
            )
            .bind(translation_event_id)
            .bind("meeting-translation")
            .bind("utterance-source-priority")
            .bind(source_event_id)
            .bind(source_kind)
            .bind(source_hash)
            .bind(counter)
            .bind(counter)
            .bind(status)
            .bind(translated_text)
            .bind("2026-09-02T00:00:06Z")
            .execute(&pool)
            .await
            .expect("insert source-priority translation oracle row");
        }

        let priority_latest: (String, String, String) = sqlx::query_as(
            "SELECT translation_event_id, source_event_id, source_kind FROM translation_latest WHERE meeting_id = ? AND utterance_id = ? AND target_language = ?",
        )
        .bind("meeting-translation")
        .bind("utterance-source-priority")
        .bind("zh-cn")
        .fetch_one(&pool)
        .await
        .expect("read same-revision source priority projection");
        assert_eq!(
            priority_latest,
            (
                "translation-priority-final".to_string(),
                "source-priority-final".to_string(),
                "final".to_string(),
            ),
            "same-revision final must replace partial and late partial must not roll it back"
        );

        for (event_id, revision, event_kind, transcript) in [
            ("source-fresh-old", 1_i64, "final", "old source"),
            ("source-fresh-new", 2_i64, "correction", "new source"),
        ] {
            sqlx::query(
                r#"INSERT INTO utterance_revisions (
                    event_id, meeting_id, schema_version, utterance_id, revision,
                    event_kind, is_stable, transcript, timestamp, created_at
                ) VALUES (?, ?, 1, ?, ?, ?, 1, ?, ?, ?)"#,
            )
            .bind(event_id)
            .bind("meeting-translation")
            .bind("utterance-freshness")
            .bind(revision)
            .bind(event_kind)
            .bind(transcript)
            .bind("00:00:02")
            .bind("2026-09-02T00:00:05Z")
            .execute(&pool)
            .await
            .expect("insert freshness source revision");
        }

        sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, translated_text,
                provider, created_at
            ) VALUES (?, ?, ?, ?, 2, 'correction', ?, 10, 10, 'snapshot', 'final',
                      'ja', 'zh-CN', ?, 'fixture-provider', ?)"#,
        )
        .bind("translation-fresh-new")
        .bind("meeting-translation")
        .bind("utterance-freshness")
        .bind("source-fresh-new")
        .bind(HASH_B)
        .bind("current source translation")
        .bind("2026-09-02T00:00:06Z")
        .execute(&pool)
        .await
        .expect("insert newer source projection first");

        sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, translated_text,
                provider, created_at
            ) VALUES (?, ?, ?, ?, 1, 'final', ?, 11, 1, 'snapshot', 'final',
                      'ja', 'zh-CN', ?, 'fixture-provider', ?)"#,
        )
        .bind("translation-fresh-old-late")
        .bind("meeting-translation")
        .bind("utterance-freshness")
        .bind("source-fresh-old")
        .bind(HASH_A)
        .bind("late old source translation")
        .bind("2026-09-02T00:00:07Z")
        .execute(&pool)
        .await
        .expect("retain late old-source history");

        let freshness_latest: (String, i64, i64, i64) = sqlx::query_as(
            "SELECT translation_event_id, source_revision, generation, translation_revision FROM translation_latest WHERE meeting_id = ? AND utterance_id = ? AND target_language = ?",
        )
        .bind("meeting-translation")
        .bind("utterance-freshness")
        .bind("zh-cn")
        .fetch_one(&pool)
        .await
        .expect("read source-fresh latest projection");
        assert_eq!(
            freshness_latest,
            ("translation-fresh-new".to_string(), 2, 10, 10),
            "a larger translation revision from an older source must not roll latest back"
        );

        let duplicate_generation = sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, translated_text,
                provider, created_at
            ) VALUES (?, ?, ?, ?, 2, 'correction', ?, 12, 10, 'snapshot', 'final',
                      'ja', 'zh-CN', ?, 'fixture-provider', ?)"#,
        )
        .bind("translation-duplicate-generation")
        .bind("meeting-translation")
        .bind("utterance-freshness")
        .bind("source-fresh-new")
        .bind(HASH_B)
        .bind("invalid generation reuse")
        .bind("2026-09-02T00:00:08Z")
        .execute(&pool)
        .await;
        assert!(
            duplicate_generation.is_err(),
            "one scope cannot reuse a generation with a larger translation revision"
        );

        let duplicate_translation_revision = sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, translated_text,
                provider, created_at
            ) VALUES (?, ?, ?, ?, 2, 'correction', ?, 10, 12, 'snapshot', 'final',
                      'ja', 'zh-CN', ?, 'fixture-provider', ?)"#,
        )
        .bind("translation-duplicate-revision")
        .bind("meeting-translation")
        .bind("utterance-freshness")
        .bind("source-fresh-new")
        .bind(HASH_B)
        .bind("invalid revision reuse")
        .bind("2026-09-02T00:00:08Z")
        .execute(&pool)
        .await;
        assert!(
            duplicate_translation_revision.is_err(),
            "one scope cannot reuse a translation revision with a larger generation"
        );

        sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, translated_text,
                provider, created_at
            ) VALUES (?, ?, ?, ?, 2, 'correction', ?, 13, 13, 'snapshot', 'final',
                      'ja', 'zh-CN', ?, 'fixture-provider', ?)"#,
        )
        .bind("translation-conflicting-source-identity")
        .bind("meeting-translation")
        .bind("utterance-freshness")
        .bind("source-fresh-new")
        .bind(HASH_A)
        .bind("invalid same-revision source identity")
        .bind("2026-09-02T00:00:08Z")
        .execute(&pool)
        .await
        .expect("retain app-invalid identity as audit history");
        let latest_after_identity_conflict: String = sqlx::query_scalar(
            "SELECT translation_event_id FROM translation_latest WHERE meeting_id = ? AND utterance_id = ? AND target_language = ?",
        )
        .bind("meeting-translation")
        .bind("utterance-freshness")
        .bind("zh-cn")
        .fetch_one(&pool)
        .await
        .expect("read latest after source identity conflict");
        assert_eq!(
            latest_after_identity_conflict, "translation-fresh-new",
            "same source revision with a different text hash cannot replace latest"
        );

        for (event_id, revision, event_kind) in [
            ("source-failed-1", 1_i64, "final"),
            ("source-failed-2", 2_i64, "speaker_update"),
        ] {
            sqlx::query(
                r#"INSERT INTO utterance_revisions (
                    event_id, meeting_id, schema_version, utterance_id, revision,
                    event_kind, is_stable, transcript, timestamp, created_at
                ) VALUES (?, ?, 1, ?, ?, ?, 1, ?, ?, ?)"#,
            )
            .bind(event_id)
            .bind("meeting-translation")
            .bind("utterance-failed-reuse")
            .bind(revision)
            .bind(event_kind)
            .bind("same source")
            .bind("00:00:03")
            .bind("2026-09-02T00:00:09Z")
            .execute(&pool)
            .await
            .expect("insert failed-reuse source revision");
        }

        sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, provider,
                error_code, error_message, retryable, created_at
            ) VALUES (?, ?, ?, ?, 1, 'final', ?, 1, 1, 'error', 'failed',
                      'ja', 'zh-CN', 'fixture-provider',
                      'provider_failed', 'safe failure', 1, ?)"#,
        )
        .bind("translation-failed-parent")
        .bind("meeting-translation")
        .bind("utterance-failed-reuse")
        .bind("source-failed-1")
        .bind(HASH_A)
        .bind("2026-09-02T00:00:10Z")
        .execute(&pool)
        .await
        .expect("insert failed translation parent");

        let reuse_failed = sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, translated_text,
                provider, reused_from_event_id, created_at
            ) VALUES (?, ?, ?, ?, 2, 'speaker_update', ?, 2, 2, 'snapshot', 'reused',
                      'ja', 'zh-CN', ?, 'fixture-provider', ?, ?)"#,
        )
        .bind("translation-invalid-reuse")
        .bind("meeting-translation")
        .bind("utterance-failed-reuse")
        .bind("source-failed-2")
        .bind(HASH_A)
        .bind("must not reuse a failure")
        .bind("translation-failed-parent")
        .bind("2026-09-02T00:00:11Z")
        .execute(&pool)
        .await;
        assert!(
            reuse_failed.is_err(),
            "reuse must reference a completed final/reused snapshot"
        );

        sqlx::query(
            r#"INSERT INTO translation_glossaries (
                glossary_id, meeting_id, name, source_language, target_language,
                created_at, updated_at
            ) VALUES (?, ?, ?, 'ja', 'zh-CN', ?, ?)"#,
        )
        .bind("glossary-db")
        .bind("meeting-translation")
        .bind("Meeting terms")
        .bind("2026-09-02T00:00:12Z")
        .bind("2026-09-02T00:00:12Z")
        .execute(&pool)
        .await
        .expect("insert glossary identity");
        for (version, content_hash) in [(1_i64, HASH_A), (2_i64, HASH_B)] {
            sqlx::query(
                r#"INSERT INTO translation_glossary_versions (
                    glossary_id, version, content_hash, entries_json, created_at
                ) VALUES (?, ?, ?, '[]', ?)"#,
            )
            .bind("glossary-db")
            .bind(version)
            .bind(content_hash)
            .bind("2026-09-02T00:00:12Z")
            .execute(&pool)
            .await
            .expect("insert immutable glossary version");
        }
        sqlx::query(
            r#"INSERT INTO utterance_revisions (
                event_id, meeting_id, schema_version, utterance_id, revision,
                event_kind, is_stable, transcript, timestamp, created_at
            ) VALUES (?, ?, 1, ?, 1, 'final', 1, ?, ?, ?)"#,
        )
        .bind("source-glossary-db")
        .bind("meeting-translation")
        .bind("utterance-glossary-db")
        .bind("glossary source")
        .bind("00:00:04")
        .bind("2026-09-02T00:00:12Z")
        .execute(&pool)
        .await
        .expect("insert glossary source");

        for (event_id, version, content_hash, counter, translated_text) in [
            ("translation-glossary-db-v1", 1_i64, HASH_A, 1_i64, "v1"),
            ("translation-glossary-db-v2", 2_i64, HASH_B, 2_i64, "v2"),
        ] {
            sqlx::query(
                r#"INSERT INTO translation_revisions (
                    translation_event_id, meeting_id, utterance_id,
                    source_event_id, source_revision, source_kind, source_text_hash,
                    translation_revision, generation, event_kind, status,
                    source_language, target_language, translated_text, provider,
                    glossary_id, glossary_version, glossary_content_hash, created_at
                ) VALUES (?, ?, ?, ?, 1, 'final', ?, ?, ?, 'snapshot', 'final',
                          'ja', 'zh-CN', ?, 'fixture-provider', ?, ?, ?, ?)"#,
            )
            .bind(event_id)
            .bind("meeting-translation")
            .bind("utterance-glossary-db")
            .bind("source-glossary-db")
            .bind(HASH_A)
            .bind(counter)
            .bind(counter)
            .bind(translated_text)
            .bind("glossary-db")
            .bind(version)
            .bind(content_hash)
            .bind("2026-09-02T00:00:13Z")
            .execute(&pool)
            .await
            .expect("same source triple accepts a newer glossary request fingerprint");
        }
        let latest_glossary_version: i64 = sqlx::query_scalar(
            "SELECT glossary_version FROM translation_latest WHERE meeting_id = ? AND utterance_id = ? AND target_language = ?",
        )
        .bind("meeting-translation")
        .bind("utterance-glossary-db")
        .bind("zh-cn")
        .fetch_one(&pool)
        .await
        .expect("read latest glossary projection");
        assert_eq!(latest_glossary_version, 2);

        let cross_scope_latest = sqlx::query(
            "UPDATE translation_latest SET translation_event_id = ? WHERE meeting_id = ? AND utterance_id = ? AND target_language = ?",
        )
        .bind("translation-fresh-old-late")
        .bind("meeting-translation")
        .bind("utterance-1")
        .bind("zh-CN")
        .execute(&pool)
        .await;
        assert!(
            cross_scope_latest.is_err(),
            "latest translation event must belong to the same scope"
        );

        let invalid_hash = sqlx::query(
            r#"INSERT INTO translation_revisions (
                translation_event_id, meeting_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                translation_revision, generation, event_kind, status,
                source_language, target_language, translated_text,
                provider, created_at
            ) VALUES (?, ?, ?, ?, 2, 'speaker_update', 'NOT-A-SHA256', 5, 5,
                      'snapshot', 'final', 'ja', 'zh-CN', ?, ?, ?)"#,
        )
        .bind("translation-invalid-hash")
        .bind("meeting-translation")
        .bind("utterance-1")
        .bind("source-speaker-2")
        .bind("invalid")
        .bind("fixture-provider")
        .bind("2026-09-02T00:00:05Z")
        .execute(&pool)
        .await;
        assert!(
            invalid_hash.is_err(),
            "invalid source hash must be rejected"
        );

        let update_result = sqlx::query(
            "UPDATE translation_revisions SET translated_text = ? WHERE translation_event_id = ?",
        )
        .bind("mutated")
        .bind("translation-event-3")
        .execute(&pool)
        .await;
        assert!(update_result.is_err(), "revision history must be immutable");

        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM pragma_table_info('translation_revisions') ORDER BY cid",
        )
        .fetch_all(&pool)
        .await
        .expect("inspect translation columns");
        assert!(
            columns.iter().all(|column| !column.contains("delta")),
            "provider token deltas must not be persisted"
        );

        sqlx::query("DELETE FROM meetings WHERE id = ?")
            .bind("meeting-translation")
            .execute(&pool)
            .await
            .expect("meeting cascade may delete revision history with reuse links");
        let remaining: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM translation_revisions) + (SELECT COUNT(*) FROM translation_latest)",
        )
        .fetch_one(&pool)
        .await
        .expect("count translation rows after cascade");
        assert_eq!(remaining, 0);
    }
}
