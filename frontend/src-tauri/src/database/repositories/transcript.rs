use crate::api::{TranscriptSearchResult, TranscriptSegment};
use crate::database::models::StoredTranscriptEvent;
use crate::database::repositories::recording_session_binding::{
    RecordingSessionBindingRepository, RecordingSessionBindingStoreError,
};
use crate::database::repositories::transcript_event::{
    TranscriptEventStoreError, TranscriptEventsRepository,
};
use chrono::Utc;
use sqlx::{Connection, Error as SqlxError, SqlitePool};
use std::collections::HashSet;
use tracing::{error, info};
use uuid::Uuid;

pub struct TranscriptsRepository;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedRecordingSaveBinding {
    pub source_session_id: String,
    pub recording_folder_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveTranscriptOutcome {
    pub meeting_id: String,
    pub reused_existing_meeting: bool,
}

impl TranscriptsRepository {
    /// Saves a new meeting and its associated transcript segments.
    /// This function uses a transaction to ensure that either both the meeting
    /// and all its transcripts are saved, or none of them are.
    pub async fn save_transcript(
        pool: &SqlitePool,
        meeting_title: &str,
        transcripts: &[TranscriptSegment],
        folder_path: Option<String>,
    ) -> Result<String, TranscriptEventStoreError> {
        Ok(Self::save_transcript_with_recording_binding(
            pool,
            meeting_title,
            transcripts,
            folder_path,
            None,
        )
        .await?
        .meeting_id)
    }

    /// Save a trusted recording and atomically complete its durable
    /// session-to-meeting correlation. If a prior process committed this exact
    /// session before crashing, return that meeting instead of creating a
    /// duplicate.
    pub async fn save_transcript_with_recording_binding(
        pool: &SqlitePool,
        meeting_title: &str,
        transcripts: &[TranscriptSegment],
        folder_path: Option<String>,
        recording_binding: Option<&TrustedRecordingSaveBinding>,
    ) -> Result<SaveTranscriptOutcome, TranscriptEventStoreError> {
        if let Some(binding) = recording_binding {
            validate_trusted_recording_segments(transcripts, &binding.source_session_id)?;
            if let Some(existing) =
                RecordingSessionBindingRepository::load_by_session(pool, &binding.source_session_id)
                    .await
                    .map_err(recording_binding_error)?
            {
                if existing.recording_folder_hash != binding.recording_folder_hash {
                    return Err(TranscriptEventStoreError::InvalidInput(
                        "recording session folder correlation changed".to_string(),
                    ));
                }
                if let Some(meeting_id) = existing.meeting_id {
                    return Ok(SaveTranscriptOutcome {
                        meeting_id,
                        reused_existing_meeting: true,
                    });
                }
            }
        }

        let meeting_id = format!("meeting-{}", Uuid::new_v4());

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let now = Utc::now();

        // 1. Create the new meeting
        let result = sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&meeting_id)
        .bind(meeting_title)
        .bind(now)
        .bind(now)
        .bind(&folder_path)
        .execute(&mut *transaction)
        .await;

        if let Err(e) = result {
            error!("Failed to create meeting record: {}", e);
            transaction.rollback().await?;
            return Err(e.into());
        }

        info!("Successfully created meeting with id: {}", meeting_id);

        if let Some(binding) = recording_binding {
            if let Err(error) = RecordingSessionBindingRepository::bind_in_transaction(
                &mut transaction,
                &binding.source_session_id,
                &binding.recording_folder_hash,
                &meeting_id,
            )
            .await
            {
                transaction.rollback().await?;
                return Err(recording_binding_error(error));
            }
        }

        // 2. Save every supplied revision. The materialized `transcripts` row
        // is updated only when a higher revision arrives, so an IndexedDB
        // export may safely contain retries and out-of-order events.
        let mut seen_event_ids = HashSet::new();
        for segment in transcripts {
            let utterance_id = segment
                .utterance_id
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| segment.id.clone());
            // Canonical live events start at revision 0. Only legacy segments
            // without an explicit revision are seeded as revision 1.
            let revision = segment.revision.unwrap_or(1);
            let is_stable = segment
                .is_stable
                .unwrap_or_else(|| !segment.is_partial.unwrap_or(false));
            let event_kind = segment
                .event_kind
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    if is_stable {
                        "final".to_string()
                    } else {
                        "partial".to_string()
                    }
                });
            let event_id = segment
                .event_id
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "legacy-event::{}::{}::{}",
                        meeting_id, utterance_id, revision
                    )
                });
            if !seen_event_ids.insert(event_id.clone()) {
                // The event ID is global delivery identity. A provider that
                // reuses it for another revision is malformed, but one bad
                // delivery must not roll back every valid utterance.
                tracing::warn!(
                    "Ignoring duplicate transcript event_id {} while saving meeting {}",
                    event_id,
                    meeting_id
                );
                continue;
            }
            let created_at = segment
                .created_at
                .as_deref()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            let sequence_id = segment
                .sequence_id
                .map(i64::try_from)
                .transpose()
                .map_err(|_| {
                    TranscriptEventStoreError::InvalidInput(
                        "sequence_id exceeds SQLite INTEGER range".to_string(),
                    )
                })?;
            let audio_start_time = segment.audio_start_time.or(segment.chunk_start_time);
            let start_ms = segment
                .start_ms
                .or_else(|| audio_start_time.map(seconds_to_milliseconds));
            let end_ms = segment
                .end_ms
                .or_else(|| segment.audio_end_time.map(seconds_to_milliseconds));
            let event = StoredTranscriptEvent {
                event_id,
                meeting_id: meeting_id.clone(),
                schema_version: segment.schema_version.unwrap_or(0),
                session_id: segment.session_id.clone(),
                utterance_id,
                revision,
                event_kind,
                is_stable,
                text: segment.text.clone(),
                timestamp: segment.timestamp.clone(),
                sequence_id,
                start_ms,
                end_ms,
                audio_start_time,
                audio_end_time: segment.audio_end_time,
                duration: segment.duration,
                audio_source: segment
                    .audio_source
                    .clone()
                    .or_else(|| segment.source.clone()),
                speaker_id: segment.speaker_id.clone(),
                speaker_local_label: segment.speaker_local_label.clone(),
                speaker_display_name: segment.speaker_display_name.clone(),
                speaker_confidence: segment.speaker_confidence.map(f64::from),
                speaker_status: segment.speaker_status.clone(),
                language: segment.language.clone(),
                asr_provider: segment.asr_provider.clone(),
                asr_model: segment.asr_model.clone(),
                asr_confidence: segment.asr_confidence.or(segment.confidence).map(f64::from),
                asr_latency_ms: segment.asr_latency_ms,
                diarization_provider: segment.diarization_provider.clone(),
                diarization_model: segment.diarization_model.clone(),
                diarization_model_revision: segment.diarization_model_revision.clone(),
                diarization_revision: segment.diarization_revision,
                diarization_window_id: segment.diarization_window_id.clone(),
                diarization_window_start_frame: segment.diarization_window_start_frame,
                diarization_window_end_frame: segment.diarization_window_end_frame,
                diarization_status: segment.diarization_status.clone(),
                diarization_latency_ms: segment.diarization_latency_ms,
                replaces_event_id: segment.replaces_event_id.clone(),
                provider_event_id: segment.provider_event_id.clone(),
                trace_id: segment.trace_id.clone(),
                created_at,
            };

            if let Err(e) =
                TranscriptEventsRepository::upsert_revision_in_transaction(&mut transaction, &event)
                    .await
            {
                error!(
                    "Failed to save transcript revision for meeting {}: {}",
                    meeting_id, e
                );
                transaction.rollback().await?;
                return Err(e);
            }
        }

        info!(
            "Successfully saved {} transcript segments for meeting {}",
            transcripts.len(),
            meeting_id
        );

        // Commit the transaction
        transaction.commit().await?;

        Ok(SaveTranscriptOutcome {
            meeting_id,
            reused_existing_meeting: false,
        })
    }

    /// Searches for a query string within the transcripts.
    /// It returns a list of matching transcripts with context.
    pub async fn search_transcripts(
        pool: &SqlitePool,
        query: &str,
    ) -> Result<Vec<TranscriptSearchResult>, SqlxError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let search_query = format!("%{}%", query.to_lowercase());

        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT m.id, m.title, t.transcript, t.timestamp
             FROM meetings m
             JOIN transcripts t ON m.id = t.meeting_id
             WHERE LOWER(t.transcript) LIKE ?",
        )
        .bind(&search_query)
        .fetch_all(pool)
        .await?;

        let results = rows
            .into_iter()
            .map(|(id, title, transcript, timestamp)| {
                let match_context = Self::get_match_context(&transcript, query);
                TranscriptSearchResult {
                    id,
                    title,
                    match_context,
                    timestamp,
                }
            })
            .collect();

        Ok(results)
    }

    /// Helper function to extract a snippet of text around the first match of a query.
    fn get_match_context(transcript: &str, query: &str) -> String {
        let transcript_lower = transcript.to_lowercase();
        let query_lower = query.to_lowercase();

        match transcript_lower.find(&query_lower) {
            Some(match_index) => {
                let start_index = match_index.saturating_sub(100);
                let end_index = (match_index + query.len() + 100).min(transcript.len());

                let mut context = String::new();
                if start_index > 0 {
                    context.push_str("...");
                }
                context.push_str(&transcript[start_index..end_index]);
                if end_index < transcript.len() {
                    context.push_str("...");
                }
                context
            }
            None => transcript.chars().take(200).collect(), // Fallback to the start of the transcript
        }
    }
}

fn validate_trusted_recording_segments(
    transcripts: &[TranscriptSegment],
    source_session_id: &str,
) -> Result<(), TranscriptEventStoreError> {
    if source_session_id.is_empty()
        || source_session_id.len() > 256
        || source_session_id.trim() != source_session_id
        || source_session_id.contains('\0')
    {
        return Err(TranscriptEventStoreError::InvalidInput(
            "trusted recording session identifier is invalid".to_string(),
        ));
    }
    if transcripts
        .iter()
        .any(|segment| segment.session_id.as_deref() != Some(source_session_id))
    {
        return Err(TranscriptEventStoreError::InvalidInput(
            "trusted recording contains mixed session identifiers".to_string(),
        ));
    }
    Ok(())
}

fn recording_binding_error(error: RecordingSessionBindingStoreError) -> TranscriptEventStoreError {
    match error {
        RecordingSessionBindingStoreError::Database(error) => {
            TranscriptEventStoreError::Database(error)
        }
        RecordingSessionBindingStoreError::InvalidSessionId
        | RecordingSessionBindingStoreError::InvalidFolderHash
        | RecordingSessionBindingStoreError::MissingPending
        | RecordingSessionBindingStoreError::SessionConflict
        | RecordingSessionBindingStoreError::FolderConflict => {
            TranscriptEventStoreError::InvalidInput(error.to_string())
        }
    }
}

fn seconds_to_milliseconds(seconds: f64) -> i64 {
    (seconds * 1_000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::repositories::recording_session_binding::RecordingSessionBindingRepository;
    use sqlx::sqlite::SqlitePoolOptions;

    const FOLDER_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("create in-memory database");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    fn binding(session_id: &str) -> TrustedRecordingSaveBinding {
        TrustedRecordingSaveBinding {
            source_session_id: session_id.to_string(),
            recording_folder_hash: FOLDER_HASH.to_string(),
        }
    }

    #[tokio::test]
    async fn trusted_session_save_is_atomic_and_crash_retry_is_idempotent() {
        let pool = test_pool().await;
        RecordingSessionBindingRepository::ensure_pending(&pool, "session-idempotent", FOLDER_HASH)
            .await
            .expect("persist pending session");

        let first = TranscriptsRepository::save_transcript_with_recording_binding(
            &pool,
            "Recovered empty meeting",
            &[],
            Some("D:/synthetic/recovered-empty".to_string()),
            Some(&binding("session-idempotent")),
        )
        .await
        .expect("first save");
        assert!(!first.reused_existing_meeting);

        let retry = TranscriptsRepository::save_transcript_with_recording_binding(
            &pool,
            "A renderer retry must not create a duplicate",
            &[],
            Some("D:/synthetic/recovered-empty".to_string()),
            Some(&binding("session-idempotent")),
        )
        .await
        .expect("idempotent retry");
        assert!(retry.reused_existing_meeting);
        assert_eq!(retry.meeting_id, first.meeting_id);

        let meeting_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meetings")
            .fetch_one(&pool)
            .await
            .expect("count meetings");
        assert_eq!(meeting_count, 1);
        let durable =
            RecordingSessionBindingRepository::load_by_session(&pool, "session-idempotent")
                .await
                .expect("load durable binding")
                .expect("binding exists");
        assert_eq!(
            durable.meeting_id.as_deref(),
            Some(first.meeting_id.as_str())
        );
        assert_eq!(durable.state, "bound");
    }

    #[tokio::test]
    async fn transcript_failure_rolls_back_meeting_and_pending_binding_transition() {
        let pool = test_pool().await;
        RecordingSessionBindingRepository::ensure_pending(&pool, "session-rollback", FOLDER_HASH)
            .await
            .expect("persist pending session");
        let invalid: TranscriptSegment = serde_json::from_value(serde_json::json!({
            "id": "invalid-segment",
            "text": "synthetic",
            "timestamp": "00:00:01",
            "event_id": "invalid-event",
            "schema_version": 1,
            "session_id": "session-rollback",
            "utterance_id": "invalid-utterance",
            "revision": -1,
            "event_kind": "final",
            "is_stable": true
        }))
        .expect("construct invalid repository fixture");

        assert!(
            TranscriptsRepository::save_transcript_with_recording_binding(
                &pool,
                "Must roll back",
                &[invalid],
                Some("D:/synthetic/rollback".to_string()),
                Some(&binding("session-rollback")),
            )
            .await
            .is_err()
        );

        let meeting_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meetings")
            .fetch_one(&pool)
            .await
            .expect("count rolled-back meetings");
        assert_eq!(meeting_count, 0);
        let pending = RecordingSessionBindingRepository::load_by_session(&pool, "session-rollback")
            .await
            .expect("load pending binding")
            .expect("pending binding exists");
        assert_eq!(pending.state, "pending");
        assert!(pending.meeting_id.is_none());
    }
}
