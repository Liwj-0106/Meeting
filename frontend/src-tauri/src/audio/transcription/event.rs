//! Versioned transcript events and deterministic replay.
//!
//! `TranscriptUpdate` is the wire payload emitted to the Tauri frontend. It
//! deliberately keeps every field from Meetily's original payload and makes
//! the versioned fields optional, so recordings made by older builds can
//! still be deserialized. `TranscriptEvent` is the normalized representation
//! used by persistence, streaming providers, translation, and live summary
//! consumers.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const TRANSCRIPT_EVENT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEventKind {
    Partial,
    Final,
    Correction,
    SpeakerUpdate,
    LanguageUpdate,
    Retraction,
    Unknown(String),
}

impl TranscriptEventKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Partial => "partial",
            Self::Final => "final",
            Self::Correction => "correction",
            Self::SpeakerUpdate => "speaker_update",
            Self::LanguageUpdate => "language_update",
            Self::Retraction => "retraction",
            Self::Unknown(value) => value,
        }
    }

    fn replay_rank(&self) -> u8 {
        match self {
            Self::Unknown(_) => 0,
            Self::Partial => 1,
            Self::Final => 2,
            Self::Correction => 3,
            Self::SpeakerUpdate | Self::LanguageUpdate => 4,
            Self::Retraction => 5,
        }
    }
}

impl Default for TranscriptEventKind {
    fn default() -> Self {
        Self::Unknown("unknown".to_string())
    }
}

impl Serialize for TranscriptEventKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TranscriptEventKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "partial" => Self::Partial,
            "final" => Self::Final,
            "correction" => Self::Correction,
            "speaker_update" => Self::SpeakerUpdate,
            "language_update" => Self::LanguageUpdate,
            "retraction" => Self::Retraction,
            _ => Self::Unknown(value),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSource {
    Microphone,
    System,
    Mixed,
    Imported,
    #[default]
    #[serde(other)]
    Unknown,
}

impl AudioSource {
    pub fn from_legacy_label(source: &str) -> Self {
        match source.trim().to_ascii_lowercase().as_str() {
            "microphone" | "mic" => Self::Microphone,
            "system" | "system audio" | "media" => Self::System,
            // The original worker emitted `Audio` after microphone and system
            // audio had already been mixed.
            "audio" | "mixed" => Self::Mixed,
            "import" | "imported" | "file" => Self::Imported,
            _ => Self::Unknown,
        }
    }

    pub fn legacy_label(&self) -> &'static str {
        match self {
            Self::Microphone => "Microphone",
            Self::System => "System Audio",
            Self::Mixed => "Audio",
            Self::Imported => "Imported",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeakerStatus {
    Unresolved,
    Provisional,
    Resolved,
    UserConfirmed,
    Renamed,
    Merged,
    Unknown(String),
}

impl SpeakerStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unresolved => "unresolved",
            Self::Provisional => "provisional",
            Self::Resolved => "resolved",
            Self::UserConfirmed => "user_confirmed",
            Self::Renamed => "renamed",
            Self::Merged => "merged",
            Self::Unknown(value) => value,
        }
    }
}

impl Default for SpeakerStatus {
    fn default() -> Self {
        Self::Unresolved
    }
}

impl Serialize for SpeakerStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SpeakerStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "unresolved" => Self::Unresolved,
            "provisional" => Self::Provisional,
            "resolved" => Self::Resolved,
            "user_confirmed" => Self::UserConfirmed,
            "renamed" => Self::Renamed,
            "merged" => Self::Merged,
            _ => Self::Unknown(value),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerMetadata {
    pub speaker_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub status: SpeakerStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AsrMetadata {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// Processing state for an independent speaker-diarization pass.
///
/// This is intentionally separate from `SpeakerStatus`: the former describes
/// one worker window, while the latter describes the user-visible speaker
/// identity attached to an utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiarizationStatus {
    Pending,
    Running,
    Provisional,
    Resolved,
    Failed,
    Unknown(String),
}

impl DiarizationStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Provisional => "provisional",
            Self::Resolved => "resolved",
            Self::Failed => "failed",
            Self::Unknown(value) => value,
        }
    }
}

impl Default for DiarizationStatus {
    fn default() -> Self {
        Self::Pending
    }
}

impl Serialize for DiarizationStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DiarizationStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "provisional" => Self::Provisional,
            "resolved" => Self::Resolved,
            "failed" => Self::Failed,
            _ => Self::Unknown(value),
        })
    }
}

/// Provenance for a diarization correction. ASR provenance remains in `asr`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiarizationMetadata {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_revision: Option<String>,
    /// Revision of the independent diarization pass, not the utterance event.
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    /// Bounds in Meetily's canonical 48 kHz frame timebase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start_frame: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_end_frame: Option<u64>,
    #[serde(default)]
    pub status: DiarizationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// Canonical, replay-safe transcript event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEvent {
    pub schema_version: u16,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub utterance_id: String,
    pub revision: u64,
    pub event_kind: TranscriptEventKind,
    pub is_stable: bool,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub audio_source: AudioSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<SpeakerMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr: Option<AsrMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization: Option<DiarizationMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_event_id: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Compatibility ordering hint for pre-versioned transcript consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_id: Option<u64>,
}

/// Backward-compatible Tauri payload.
///
/// Existing fields intentionally remain concrete values. Every versioned
/// field has a serde default so an old JSON payload can be parsed unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptUpdate {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub sequence_id: u64,
    #[serde(default)]
    pub chunk_start_time: f64,
    #[serde(default)]
    pub is_partial: bool,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub audio_start_time: f64,
    #[serde(default)]
    pub audio_end_time: f64,
    #[serde(default)]
    pub duration: f64,

    #[serde(default)]
    pub schema_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utterance_id: Option<String>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_kind: Option<TranscriptEventKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_stable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_source: Option<AudioSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<SpeakerMetadata>,
    /// Flattened speaker fields keep database/event consumers simple while
    /// `speaker` remains the canonical grouped representation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_local_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_status: Option<SpeakerStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr: Option<AsrMetadata>,
    /// Flattened ASR fields mirror `asr` for storage hot paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization: Option<DiarizationMetadata>,
    /// Flattened diarization fields mirror `diarization` for storage hot paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_model_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_start_frame: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_end_frame: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_status: Option<DiarizationStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Inputs which the chunk worker knows at emission time. Identity, schema,
/// stability, wall-clock timestamps, and duration are derived automatically.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptChunkInput {
    pub text: String,
    pub sequence_id: u64,
    pub audio_start_time: f64,
    pub audio_end_time: f64,
    pub is_partial: bool,
    pub confidence: Option<f32>,
    pub source: AudioSource,
    pub provider: String,
    pub model: Option<String>,
    pub meeting_id: Option<String>,
    pub session_id: Option<String>,
    pub language: Option<String>,
    pub trace_id: Option<String>,
}

impl TranscriptChunkInput {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: impl Into<String>,
        sequence_id: u64,
        audio_start_time: f64,
        audio_end_time: f64,
        is_partial: bool,
        confidence: Option<f32>,
        source: AudioSource,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            text: text.into(),
            sequence_id,
            audio_start_time,
            audio_end_time,
            is_partial,
            confidence,
            source,
            provider: provider.into(),
            model: None,
            meeting_id: None,
            session_id: None,
            language: None,
            trace_id: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptNormalizationContext {
    pub meeting_id: Option<String>,
    pub session_id: Option<String>,
    pub language: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub trace_id: Option<String>,
}

impl TranscriptUpdate {
    /// Build a versioned payload from the original chunk-worker inputs.
    pub fn from_legacy_chunk(input: TranscriptChunkInput) -> Self {
        let now = Utc::now();
        let created_at = now.to_rfc3339_opts(SecondsFormat::Millis, true);
        let timestamp = now.format("%H:%M:%S").to_string();
        let start_ms = seconds_to_ms(input.audio_start_time);
        let end_ms = seconds_to_ms(input.audio_end_time).max(start_ms);
        let session_key = input.session_id.as_deref().unwrap_or("legacy");
        let utterance_id = format!("{}:{}", session_key, input.sequence_id);
        let event_kind = if input.is_partial {
            TranscriptEventKind::Partial
        } else {
            TranscriptEventKind::Final
        };
        let confidence = input.confidence.unwrap_or(0.85);

        Self {
            text: input.text,
            timestamp,
            source: input.source.legacy_label().to_string(),
            sequence_id: input.sequence_id,
            chunk_start_time: input.audio_start_time,
            is_partial: input.is_partial,
            confidence,
            audio_start_time: input.audio_start_time,
            audio_end_time: input.audio_end_time,
            duration: (input.audio_end_time - input.audio_start_time).max(0.0),
            schema_version: TRANSCRIPT_EVENT_SCHEMA_VERSION,
            event_id: Some(Uuid::new_v4().to_string()),
            meeting_id: input.meeting_id,
            session_id: input.session_id,
            utterance_id: Some(utterance_id),
            revision: 0,
            event_kind: Some(event_kind),
            is_stable: Some(!input.is_partial),
            start_ms: Some(start_ms),
            end_ms: Some(end_ms),
            language: input.language,
            audio_source: Some(input.source),
            speaker: None,
            speaker_id: None,
            speaker_local_label: None,
            speaker_display_name: None,
            speaker_confidence: None,
            speaker_status: None,
            asr: Some(AsrMetadata {
                provider: input.provider.clone(),
                model: input.model.clone(),
                confidence: input.confidence,
                latency_ms: None,
            }),
            asr_provider: Some(input.provider),
            asr_model: input.model,
            asr_confidence: input.confidence,
            asr_latency_ms: None,
            diarization: None,
            diarization_provider: None,
            diarization_model: None,
            diarization_model_revision: None,
            diarization_revision: None,
            diarization_window_id: None,
            diarization_window_start_frame: None,
            diarization_window_end_frame: None,
            diarization_status: None,
            diarization_latency_ms: None,
            replaces_event_id: None,
            provider_event_id: None,
            created_at: Some(created_at),
            trace_id: input.trace_id,
        }
    }

    /// Normalize either a versioned update or an old Meetily payload.
    pub fn normalize(self) -> TranscriptEvent {
        self.normalize_with(&TranscriptNormalizationContext::default())
    }

    pub fn normalize_with(self, context: &TranscriptNormalizationContext) -> TranscriptEvent {
        let meeting_id = self.meeting_id.or_else(|| context.meeting_id.clone());
        let session_id = self.session_id.or_else(|| context.session_id.clone());
        let session_key = session_id.as_deref().unwrap_or("legacy");
        let utterance_id = self
            .utterance_id
            .unwrap_or_else(|| format!("{}:{}", session_key, self.sequence_id));
        let event_kind = self.event_kind.unwrap_or(if self.is_partial {
            TranscriptEventKind::Partial
        } else {
            TranscriptEventKind::Final
        });
        let start_ms = self
            .start_ms
            .unwrap_or_else(|| seconds_to_ms(self.audio_start_time));
        let end_ms = self
            .end_ms
            .unwrap_or_else(|| seconds_to_ms(self.audio_end_time))
            .max(start_ms);
        let audio_source = self
            .audio_source
            .unwrap_or_else(|| AudioSource::from_legacy_label(&self.source));
        let speaker = match self.speaker {
            Some(mut speaker) => {
                if speaker.local_label.is_none() {
                    speaker.local_label = self.speaker_local_label.clone();
                }
                if speaker.display_name.is_none() {
                    speaker.display_name = self.speaker_display_name.clone();
                }
                if speaker.confidence.is_none() {
                    speaker.confidence = self.speaker_confidence;
                }
                Some(speaker)
            }
            None => self.speaker_id.map(|speaker_id| SpeakerMetadata {
                speaker_id,
                local_label: self.speaker_local_label,
                display_name: self.speaker_display_name,
                confidence: self.speaker_confidence,
                status: self.speaker_status.unwrap_or_default(),
            }),
        };
        let asr = match self.asr {
            Some(mut asr) => {
                if asr.provider.is_empty() {
                    asr.provider = self
                        .asr_provider
                        .clone()
                        .or_else(|| context.provider.clone())
                        .unwrap_or_else(|| "legacy".to_string());
                }
                if asr.model.is_none() {
                    asr.model = self.asr_model.clone().or_else(|| context.model.clone());
                }
                if asr.confidence.is_none() {
                    asr.confidence = self.asr_confidence.or(Some(self.confidence));
                }
                if asr.latency_ms.is_none() {
                    asr.latency_ms = self.asr_latency_ms;
                }
                Some(asr)
            }
            None => Some(AsrMetadata {
                provider: self
                    .asr_provider
                    .clone()
                    .or_else(|| context.provider.clone())
                    .unwrap_or_else(|| "legacy".to_string()),
                model: self.asr_model.clone().or_else(|| context.model.clone()),
                confidence: self.asr_confidence.or(Some(self.confidence)),
                latency_ms: self.asr_latency_ms,
            }),
        };
        let diarization = match self.diarization {
            Some(mut diarization) => {
                if diarization.provider.is_empty() {
                    if let Some(provider) = self.diarization_provider.clone() {
                        diarization.provider = provider;
                    }
                }
                if diarization.model.is_none() {
                    diarization.model = self.diarization_model.clone();
                }
                if diarization.model_revision.is_none() {
                    diarization.model_revision = self.diarization_model_revision.clone();
                }
                if diarization.window_id.is_none() {
                    diarization.window_id = self.diarization_window_id.clone();
                }
                if diarization.window_start_frame.is_none() {
                    diarization.window_start_frame = self.diarization_window_start_frame;
                }
                if diarization.window_end_frame.is_none() {
                    diarization.window_end_frame = self.diarization_window_end_frame;
                }
                if diarization.latency_ms.is_none() {
                    diarization.latency_ms = self.diarization_latency_ms;
                }
                Some(diarization)
            }
            None => {
                let has_flat_diarization = self.diarization_provider.is_some()
                    || self.diarization_model.is_some()
                    || self.diarization_model_revision.is_some()
                    || self.diarization_revision.is_some()
                    || self.diarization_window_id.is_some()
                    || self.diarization_window_start_frame.is_some()
                    || self.diarization_window_end_frame.is_some()
                    || self.diarization_status.is_some()
                    || self.diarization_latency_ms.is_some();
                has_flat_diarization.then(|| DiarizationMetadata {
                    provider: self.diarization_provider.clone().unwrap_or_default(),
                    model: self.diarization_model.clone(),
                    model_revision: self.diarization_model_revision.clone(),
                    revision: self.diarization_revision.unwrap_or_default(),
                    window_id: self.diarization_window_id.clone(),
                    window_start_frame: self.diarization_window_start_frame,
                    window_end_frame: self.diarization_window_end_frame,
                    status: self.diarization_status.unwrap_or_default(),
                    latency_ms: self.diarization_latency_ms,
                })
            }
        };
        let created_at = self.created_at.unwrap_or_else(|| {
            if self.timestamp.is_empty() {
                "legacy".to_string()
            } else {
                self.timestamp.clone()
            }
        });
        let event_id = self.event_id.unwrap_or_else(|| {
            let fingerprint =
                transcript_event_fingerprint(&self.text, start_ms, end_ms, event_kind.as_str());
            format!(
                "legacy:{}:revision:{}:sequence:{}:{:016x}",
                utterance_id, self.revision, self.sequence_id, fingerprint
            )
        });

        TranscriptEvent {
            schema_version: self.schema_version,
            event_id,
            meeting_id,
            session_id,
            utterance_id,
            revision: self.revision,
            event_kind,
            is_stable: self.is_stable.unwrap_or(!self.is_partial),
            start_ms,
            end_ms,
            text: self.text,
            language: self.language.or_else(|| context.language.clone()),
            audio_source,
            speaker,
            asr,
            diarization,
            replaces_event_id: self.replaces_event_id,
            provider_event_id: self.provider_event_id,
            created_at,
            trace_id: self.trace_id.or_else(|| context.trace_id.clone()),
            sequence_id: Some(self.sequence_id),
        }
    }
}

fn seconds_to_ms(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }

    let milliseconds = seconds * 1_000.0;
    if milliseconds >= u64::MAX as f64 {
        u64::MAX
    } else {
        milliseconds.round() as u64
    }
}

fn transcript_event_fingerprint(text: &str, start_ms: u64, end_ms: u64, kind: &str) -> u64 {
    // Stable FNV-1a avoids a randomized process hash while keeping fallback
    // event IDs compact. Explicit provider event IDs always take precedence.
    let mut hash = 0xcbf29ce484222325_u64;
    let start_bytes = start_ms.to_le_bytes();
    let end_bytes = end_ms.to_le_bytes();
    for byte in text
        .as_bytes()
        .iter()
        .chain(start_bytes.iter())
        .chain(end_bytes.iter())
        .chain(kind.as_bytes().iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayApplyResult {
    Added,
    Updated { previous_revision: u64 },
    Duplicate,
    IgnoredStale { current_revision: u64 },
    ConflictReplaced,
    ConflictIgnored,
}

/// Materialized transcript state produced by replaying the append-only event
/// stream. Applying the same events repeatedly is safe.
#[derive(Debug, Default)]
pub struct TranscriptReplay {
    events_by_utterance: HashMap<String, TranscriptEvent>,
    seen_event_ids: HashSet<String>,
}

impl TranscriptReplay {
    pub fn apply_update(
        &mut self,
        update: TranscriptUpdate,
        context: &TranscriptNormalizationContext,
    ) -> ReplayApplyResult {
        self.apply(update.normalize_with(context))
    }

    pub fn apply(&mut self, event: TranscriptEvent) -> ReplayApplyResult {
        if !self.seen_event_ids.insert(event.event_id.clone()) {
            return ReplayApplyResult::Duplicate;
        }

        let Some(current) = self.events_by_utterance.get(&event.utterance_id) else {
            self.events_by_utterance
                .insert(event.utterance_id.clone(), event);
            return ReplayApplyResult::Added;
        };

        if event.revision < current.revision {
            return ReplayApplyResult::IgnoredStale {
                current_revision: current.revision,
            };
        }

        if event.revision > current.revision {
            let previous_revision = current.revision;
            self.events_by_utterance
                .insert(event.utterance_id.clone(), event);
            return ReplayApplyResult::Updated { previous_revision };
        }

        if same_revision_wins(&event, current) {
            self.events_by_utterance
                .insert(event.utterance_id.clone(), event);
            ReplayApplyResult::ConflictReplaced
        } else {
            ReplayApplyResult::ConflictIgnored
        }
    }

    pub fn replay(events: impl IntoIterator<Item = TranscriptEvent>) -> Self {
        let mut replay = Self::default();
        for event in events {
            replay.apply(event);
        }
        replay
    }

    pub fn len(&self) -> usize {
        self.events_by_utterance.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events_by_utterance.is_empty()
    }

    pub fn get(&self, utterance_id: &str) -> Option<&TranscriptEvent> {
        self.events_by_utterance.get(utterance_id)
    }

    /// Chronological snapshot with deterministic tie-breaking.
    pub fn snapshot(&self) -> Vec<TranscriptEvent> {
        let mut events: Vec<_> = self.events_by_utterance.values().cloned().collect();
        events.sort_by(|left, right| {
            (
                left.start_ms,
                left.end_ms,
                left.sequence_id.unwrap_or(u64::MAX),
                left.utterance_id.as_str(),
            )
                .cmp(&(
                    right.start_ms,
                    right.end_ms,
                    right.sequence_id.unwrap_or(u64::MAX),
                    right.utterance_id.as_str(),
                ))
        });
        events
    }

    pub fn stable_snapshot(&self) -> Vec<TranscriptEvent> {
        self.snapshot()
            .into_iter()
            .filter(|event| event.is_stable)
            .collect()
    }
}

fn same_revision_wins(incoming: &TranscriptEvent, current: &TranscriptEvent) -> bool {
    (
        incoming.is_stable as u8,
        incoming.event_kind.replay_rank(),
        incoming.created_at.as_str(),
        incoming.event_id.as_str(),
    ) > (
        current.is_stable as u8,
        current.event_kind.replay_rank(),
        current.created_at.as_str(),
        current.event_id.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(
        utterance_id: &str,
        event_id: &str,
        revision: u64,
        text: &str,
        is_stable: bool,
    ) -> TranscriptEvent {
        TranscriptEvent {
            schema_version: TRANSCRIPT_EVENT_SCHEMA_VERSION,
            event_id: event_id.to_string(),
            meeting_id: Some("meeting-1".to_string()),
            session_id: Some("session-1".to_string()),
            utterance_id: utterance_id.to_string(),
            revision,
            event_kind: if is_stable {
                TranscriptEventKind::Final
            } else {
                TranscriptEventKind::Partial
            },
            is_stable,
            start_ms: 1_000,
            end_ms: 2_000,
            text: text.to_string(),
            language: Some("zh".to_string()),
            audio_source: AudioSource::Mixed,
            speaker: None,
            asr: Some(AsrMetadata {
                provider: "test".to_string(),
                model: Some("fixture".to_string()),
                confidence: Some(0.9),
                latency_ms: Some(10),
            }),
            diarization: None,
            replaces_event_id: None,
            provider_event_id: None,
            created_at: format!("2026-01-01T00:00:0{}Z", revision.min(9)),
            trace_id: Some("trace-1".to_string()),
            sequence_id: Some(1),
        }
    }

    #[test]
    fn ignores_out_of_order_older_revision() {
        let mut replay = TranscriptReplay::default();
        assert_eq!(
            replay.apply(event("utterance-1", "event-2", 2, "最终文本", true)),
            ReplayApplyResult::Added
        );
        assert_eq!(
            replay.apply(event("utterance-1", "event-1", 1, "旧文本", false)),
            ReplayApplyResult::IgnoredStale {
                current_revision: 2
            }
        );
        assert_eq!(replay.get("utterance-1").unwrap().text, "最终文本");
    }

    #[test]
    fn duplicate_event_is_idempotent() {
        let mut replay = TranscriptReplay::default();
        let update = event("utterance-1", "event-1", 0, "你好", false);
        assert_eq!(replay.apply(update.clone()), ReplayApplyResult::Added);
        assert_eq!(replay.apply(update), ReplayApplyResult::Duplicate);
        assert_eq!(replay.len(), 1);
    }

    #[test]
    fn higher_revision_replaces_partial_with_final() {
        let mut replay = TranscriptReplay::default();
        replay.apply(event("utterance-1", "event-1", 0, "你", false));
        assert_eq!(
            replay.apply(event("utterance-1", "event-2", 1, "你好", true)),
            ReplayApplyResult::Updated {
                previous_revision: 0
            }
        );

        let stored = replay.get("utterance-1").unwrap();
        assert_eq!(stored.revision, 1);
        assert_eq!(stored.text, "你好");
        assert!(stored.is_stable);
    }

    #[test]
    fn same_revision_conflict_has_order_independent_result() {
        let earlier = event("utterance-1", "event-a", 1, "候选 A", true);
        let later = event("utterance-1", "event-b", 1, "候选 B", true);

        let first = TranscriptReplay::replay([earlier.clone(), later.clone()]);
        let second = TranscriptReplay::replay([later, earlier]);

        assert_eq!(first.snapshot(), second.snapshot());
        assert_eq!(first.get("utterance-1").unwrap().text, "候选 B");
    }

    #[test]
    fn deserializes_and_replays_legacy_payload() {
        let value = json!({
            "text": "旧版字幕",
            "timestamp": "14:30:05",
            "source": "Audio",
            "sequence_id": 7,
            "chunk_start_time": 1.25,
            "is_partial": false,
            "confidence": 0.88,
            "audio_start_time": 1.25,
            "audio_end_time": 2.5,
            "duration": 1.25
        });
        let update: TranscriptUpdate = serde_json::from_value(value).unwrap();
        let context = TranscriptNormalizationContext {
            meeting_id: Some("meeting-legacy".to_string()),
            session_id: Some("session-legacy".to_string()),
            provider: Some("local-whisper".to_string()),
            ..Default::default()
        };

        let normalized = update.clone().normalize_with(&context);
        assert_eq!(normalized.schema_version, 0);
        assert!(normalized
            .event_id
            .starts_with("legacy:session-legacy:7:revision:0:sequence:7:"));
        assert_eq!(normalized.utterance_id, "session-legacy:7");
        assert_eq!(normalized.start_ms, 1_250);
        assert_eq!(normalized.end_ms, 2_500);
        assert_eq!(normalized.audio_source, AudioSource::Mixed);
        assert!(normalized.is_stable);

        let mut replay = TranscriptReplay::default();
        assert_eq!(
            replay.apply_update(update.clone(), &context),
            ReplayApplyResult::Added
        );
        assert_eq!(
            replay.apply_update(update, &context),
            ReplayApplyResult::Duplicate
        );
        assert_eq!(replay.stable_snapshot().len(), 1);
    }

    #[test]
    fn chunk_constructor_populates_versioned_and_legacy_fields() {
        let mut input = TranscriptChunkInput::new(
            "测试字幕",
            42,
            1.5,
            2.75,
            false,
            Some(0.93),
            AudioSource::System,
            "local-whisper",
        );
        input.session_id = Some("session-42".to_string());
        input.language = Some("zh".to_string());
        input.model = Some("small-q5_1".to_string());

        let update = TranscriptUpdate::from_legacy_chunk(input);
        assert_eq!(update.schema_version, TRANSCRIPT_EVENT_SCHEMA_VERSION);
        assert_eq!(update.source, "System Audio");
        assert_eq!(update.utterance_id.as_deref(), Some("session-42:42"));
        assert_eq!(update.event_kind, Some(TranscriptEventKind::Final));
        assert_eq!(update.is_stable, Some(true));
        assert_eq!(update.start_ms, Some(1_500));
        assert_eq!(update.end_ms, Some(2_750));
        assert_eq!(update.duration, 1.25);
        assert!(update.event_id.is_some());
        assert!(update.created_at.is_some());
        assert_eq!(update.asr_provider.as_deref(), Some("local-whisper"));
        assert_eq!(update.asr_model.as_deref(), Some("small-q5_1"));
        assert_eq!(update.asr_confidence, Some(0.93));

        let normalized = update.normalize();
        assert_eq!(normalized.asr.unwrap().provider, "local-whisper");
    }

    #[test]
    fn flat_metadata_normalizes_to_canonical_nested_metadata() {
        let value = json!({
            "text": "说话人测试",
            "sequence_id": 3,
            "audio_start_time": 3.0,
            "audio_end_time": 4.0,
            "speaker_id": "speaker-local-1",
            "speaker_confidence": 0.81,
            "speaker_status": "resolved",
            "asr_provider": "cloud-stream",
            "asr_model": "fixture-v1",
            "asr_confidence": 0.96,
            "asr_latency_ms": 420,
            "diarization_provider": "moss-worker",
            "diarization_model": "MOSS-Transcribe-Diarize",
            "diarization_model_revision": "fixture-sha",
            "diarization_revision": 2,
            "diarization_window_id": "window-3",
            "diarization_window_start_frame": 144000,
            "diarization_window_end_frame": 192000,
            "diarization_status": "provisional"
        });
        let update: TranscriptUpdate = serde_json::from_value(value).unwrap();
        let event = update.normalize();

        let speaker = event.speaker.unwrap();
        assert_eq!(speaker.speaker_id, "speaker-local-1");
        assert_eq!(speaker.confidence, Some(0.81));
        assert_eq!(speaker.status, SpeakerStatus::Resolved);

        let asr = event.asr.unwrap();
        assert_eq!(asr.provider, "cloud-stream");
        assert_eq!(asr.model.as_deref(), Some("fixture-v1"));
        assert_eq!(asr.confidence, Some(0.96));
        assert_eq!(asr.latency_ms, Some(420));

        let diarization = event.diarization.unwrap();
        assert_eq!(diarization.provider, "moss-worker");
        assert_eq!(
            diarization.model.as_deref(),
            Some("MOSS-Transcribe-Diarize")
        );
        assert_eq!(diarization.model_revision.as_deref(), Some("fixture-sha"));
        assert_eq!(diarization.revision, 2);
        assert_eq!(diarization.window_id.as_deref(), Some("window-3"));
        assert_eq!(diarization.status, DiarizationStatus::Provisional);
    }

    #[test]
    fn fallback_event_id_is_revision_scoped() {
        let base = json!({
            "text": "可修订文本",
            "sequence_id": 9,
            "utterance_id": "utterance-9",
            "audio_start_time": 1.0,
            "audio_end_time": 2.0,
            "is_partial": true
        });
        let first: TranscriptUpdate = serde_json::from_value(base.clone()).unwrap();
        let mut final_value = base;
        final_value["revision"] = json!(1);
        final_value["is_partial"] = json!(false);
        final_value["event_kind"] = json!("final");
        let second: TranscriptUpdate = serde_json::from_value(final_value).unwrap();

        let first = first.normalize();
        let second = second.normalize();
        assert_ne!(first.event_id, second.event_id);

        let mut replay = TranscriptReplay::default();
        assert_eq!(replay.apply(first), ReplayApplyResult::Added);
        assert_eq!(
            replay.apply(second),
            ReplayApplyResult::Updated {
                previous_revision: 0
            }
        );
    }

    #[test]
    fn unknown_protocol_values_round_trip_without_data_loss() {
        let kind: TranscriptEventKind = serde_json::from_str("\"provider_delta\"").unwrap();
        assert_eq!(kind.as_str(), "provider_delta");
        assert_eq!(serde_json::to_string(&kind).unwrap(), "\"provider_delta\"");

        let status: SpeakerStatus = serde_json::from_str("\"voiceprint_pending\"").unwrap();
        assert_eq!(status.as_str(), "voiceprint_pending");
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            "\"voiceprint_pending\""
        );

        let diarization_status: DiarizationStatus =
            serde_json::from_str("\"provider_queued\"").unwrap();
        assert_eq!(diarization_status.as_str(), "provider_queued");
        assert_eq!(
            serde_json::to_string(&diarization_status).unwrap(),
            "\"provider_queued\""
        );
    }
}
