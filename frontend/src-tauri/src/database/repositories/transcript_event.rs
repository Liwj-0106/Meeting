use crate::database::models::{StoredTranscriptEvent, Transcript, UtteranceRevision};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, SqlitePool, Transaction};

/// Result of applying one normalized transcript event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TranscriptUpsertOutcome {
    /// The event became the materialized latest transcript.
    Applied { previous_revision: Option<i64> },
    /// The same immutable revision was already recorded.
    Duplicate { revision: i64 },
    /// The revision was added to history, but a newer materialized revision
    /// already existed and was deliberately not overwritten.
    Stale {
        revision: i64,
        current_revision: i64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum TranscriptEventStoreError {
    #[error("invalid transcript event: {0}")]
    InvalidInput(String),
    #[error("transcript event ID was reused with a different immutable payload: {event_id}")]
    EventIdConflict { event_id: String },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct TranscriptEventsRepository;

impl TranscriptEventsRepository {
    /// Atomically append an immutable revision and update the backwards-
    /// compatible `transcripts` materialized view when it is newer.
    pub async fn upsert_revision(
        pool: &SqlitePool,
        event: &StoredTranscriptEvent,
    ) -> Result<TranscriptUpsertOutcome, TranscriptEventStoreError> {
        let mut transaction = pool.begin().await?;
        let outcome = Self::upsert_revision_in_transaction(&mut transaction, event).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    /// Transaction-aware variant for callers which create a meeting and seed
    /// its final transcripts in one atomic operation.
    pub(crate) async fn upsert_revision_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        event: &StoredTranscriptEvent,
    ) -> Result<TranscriptUpsertOutcome, TranscriptEventStoreError> {
        validate_event(event)?;

        let existing = sqlx::query_as::<_, UtteranceRevision>(
            r#"
            SELECT
                event_id, meeting_id, schema_version, session_id, utterance_id, revision,
                event_kind, is_stable, transcript, timestamp,
                sequence_id, start_ms, end_ms,
                audio_start_time, audio_end_time, duration, audio_source,
                speaker_id, speaker_local_label, speaker_display_name,
                speaker_confidence, speaker_status, language,
                asr_provider, asr_model, asr_confidence, asr_latency_ms,
                diarization_provider, diarization_model, diarization_model_revision,
                diarization_revision, diarization_window_id,
                diarization_window_start_frame, diarization_window_end_frame,
                diarization_status, diarization_latency_ms,
                replaces_event_id, provider_event_id, trace_id, created_at
            FROM utterance_revisions
            WHERE event_id = ?
            "#,
        )
        .bind(&event.event_id)
        .fetch_optional(&mut **transaction)
        .await?;

        if let Some(existing) = existing {
            if revision_payload_matches(&existing, event) {
                return Ok(TranscriptUpsertOutcome::Duplicate {
                    revision: event.revision,
                });
            }

            return Err(TranscriptEventStoreError::EventIdConflict {
                event_id: event.event_id.clone(),
            });
        }

        let previous_history_head =
            latest_history_head(transaction, &event.meeting_id, &event.utterance_id).await?;

        sqlx::query(
            r#"
            INSERT INTO utterance_revisions (
                event_id, meeting_id, schema_version, session_id, utterance_id, revision,
                event_kind, is_stable, transcript, timestamp,
                sequence_id, start_ms, end_ms,
                audio_start_time, audio_end_time, duration, audio_source,
                speaker_id, speaker_local_label, speaker_display_name,
                speaker_confidence, speaker_status, language,
                asr_provider, asr_model, asr_confidence, asr_latency_ms,
                diarization_provider, diarization_model, diarization_model_revision,
                diarization_revision, diarization_window_id,
                diarization_window_start_frame, diarization_window_end_frame,
                diarization_status, diarization_latency_ms,
                replaces_event_id, provider_event_id, trace_id, created_at
            ) VALUES (
                ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?,
                ?, ?, ?,
                ?, ?, ?, ?,
                ?, ?, ?,
                ?, ?, ?,
                ?, ?, ?, ?, 
                ?, ?, ?, ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?
            )
            "#,
        )
        .bind(&event.event_id)
        .bind(&event.meeting_id)
        .bind(event.schema_version)
        .bind(&event.session_id)
        .bind(&event.utterance_id)
        .bind(event.revision)
        .bind(&event.event_kind)
        .bind(event.is_stable)
        .bind(&event.text)
        .bind(&event.timestamp)
        .bind(event.sequence_id)
        .bind(event.start_ms)
        .bind(event.end_ms)
        .bind(event.audio_start_time)
        .bind(event.audio_end_time)
        .bind(event.duration)
        .bind(&event.audio_source)
        .bind(&event.speaker_id)
        .bind(&event.speaker_local_label)
        .bind(&event.speaker_display_name)
        .bind(event.speaker_confidence)
        .bind(&event.speaker_status)
        .bind(&event.language)
        .bind(&event.asr_provider)
        .bind(&event.asr_model)
        .bind(event.asr_confidence)
        .bind(event.asr_latency_ms)
        .bind(&event.diarization_provider)
        .bind(&event.diarization_model)
        .bind(&event.diarization_model_revision)
        .bind(event.diarization_revision)
        .bind(&event.diarization_window_id)
        .bind(event.diarization_window_start_frame)
        .bind(event.diarization_window_end_frame)
        .bind(&event.diarization_status)
        .bind(event.diarization_latency_ms)
        .bind(&event.replaces_event_id)
        .bind(&event.provider_event_id)
        .bind(&event.trace_id)
        .bind(event.created_at)
        .execute(&mut **transaction)
        .await?;

        let current_materialized = sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT revision, latest_event_id FROM transcripts WHERE meeting_id = ? AND utterance_id = ?",
        )
        .bind(&event.meeting_id)
        .bind(&event.utterance_id)
        .fetch_optional(&mut **transaction)
        .await?;

        // History is the authority even after a retraction removes the
        // materialized row. Equal revisions are ordered exactly like the
        // canonical replay reducer: stability, event-kind rank, created_at,
        // then event_id. This lets a final replace its partial at revision N
        // without sacrificing immutable event-id audit history.
        let latest_history =
            latest_history_head(transaction, &event.meeting_id, &event.utterance_id)
                .await?
                .expect("the inserted transcript event is present in history");

        if latest_history.event_id != event.event_id {
            return Ok(TranscriptUpsertOutcome::Stale {
                revision: event.revision,
                current_revision: latest_history.revision,
            });
        }

        if event.event_kind.eq_ignore_ascii_case("retraction") {
            sqlx::query("DELETE FROM transcripts WHERE meeting_id = ? AND utterance_id = ?")
                .bind(&event.meeting_id)
                .bind(&event.utterance_id)
                .execute(&mut **transaction)
                .await?;

            return Ok(TranscriptUpsertOutcome::Applied {
                previous_revision: current_materialized
                    .as_ref()
                    .map(|row| row.0)
                    .or(previous_history_head.map(|head| head.revision)),
            });
        }

        match current_materialized {
            None => {
                insert_materialized_transcript(transaction, event).await?;
                Ok(TranscriptUpsertOutcome::Applied {
                    previous_revision: previous_history_head.map(|head| head.revision),
                })
            }
            Some((previous_revision, latest_event_id)) => {
                if latest_event_id.as_deref() == Some(event.event_id.as_str()) {
                    return Ok(TranscriptUpsertOutcome::Duplicate {
                        revision: event.revision,
                    });
                }
                update_materialized_transcript(transaction, event).await?;
                Ok(TranscriptUpsertOutcome::Applied {
                    previous_revision: Some(previous_revision),
                })
            }
        }
    }

    /// Read the current materialized utterances in recording order.
    pub async fn list_latest(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Vec<Transcript>, sqlx::Error> {
        sqlx::query_as::<_, Transcript>(
            r#"
            SELECT *
            FROM transcripts
            WHERE meeting_id = ?
            ORDER BY COALESCE(
                start_ms,
                CAST(ROUND(audio_start_time * 1000.0) AS INTEGER),
                9223372036854775807
            ), id
            "#,
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
    }

    /// Read every immutable revision for deterministic replay/debugging.
    pub async fn list_revisions(
        pool: &SqlitePool,
        meeting_id: &str,
        utterance_id: &str,
    ) -> Result<Vec<UtteranceRevision>, sqlx::Error> {
        sqlx::query_as::<_, UtteranceRevision>(
            r#"
            SELECT
                event_id, meeting_id, schema_version, session_id, utterance_id, revision,
                event_kind, is_stable, transcript, timestamp,
                sequence_id, start_ms, end_ms,
                audio_start_time, audio_end_time, duration, audio_source,
                speaker_id, speaker_local_label, speaker_display_name,
                speaker_confidence, speaker_status, language,
                asr_provider, asr_model, asr_confidence, asr_latency_ms,
                diarization_provider, diarization_model, diarization_model_revision,
                diarization_revision, diarization_window_id,
                diarization_window_start_frame, diarization_window_end_frame,
                diarization_status, diarization_latency_ms,
                replaces_event_id, provider_event_id, trace_id, created_at
            FROM utterance_revisions
            WHERE meeting_id = ? AND utterance_id = ?
            ORDER BY
                revision ASC,
                is_stable ASC,
                CASE lower(event_kind)
                    WHEN 'partial' THEN 1
                    WHEN 'final' THEN 2
                    WHEN 'correction' THEN 3
                    WHEN 'speaker_update' THEN 4
                    WHEN 'language_update' THEN 4
                    WHEN 'retraction' THEN 5
                    ELSE 0
                END ASC,
                created_at ASC,
                event_id ASC
            "#,
        )
        .bind(meeting_id)
        .bind(utterance_id)
        .fetch_all(pool)
        .await
    }
}

#[derive(Debug)]
struct HistoryHead {
    event_id: String,
    revision: i64,
}

async fn latest_history_head(
    transaction: &mut Transaction<'_, Sqlite>,
    meeting_id: &str,
    utterance_id: &str,
) -> Result<Option<HistoryHead>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT event_id, revision
        FROM utterance_revisions
        WHERE meeting_id = ? AND utterance_id = ?
        ORDER BY
            revision DESC,
            is_stable DESC,
            CASE lower(event_kind)
                WHEN 'partial' THEN 1
                WHEN 'final' THEN 2
                WHEN 'correction' THEN 3
                WHEN 'speaker_update' THEN 4
                WHEN 'language_update' THEN 4
                WHEN 'retraction' THEN 5
                ELSE 0
            END DESC,
            created_at DESC,
            event_id DESC
        LIMIT 1
        "#,
    )
    .bind(meeting_id)
    .bind(utterance_id)
    .fetch_optional(&mut **transaction)
    .await?;

    Ok(row.map(|(event_id, revision)| HistoryHead { event_id, revision }))
}

async fn insert_materialized_transcript(
    transaction: &mut Transaction<'_, Sqlite>,
    event: &StoredTranscriptEvent,
) -> Result<(), sqlx::Error> {
    let row_id = format!("transcript::{}::{}", event.meeting_id, event.utterance_id);
    let now = Utc::now();

    sqlx::query(
        r#"
        INSERT INTO transcripts (
            id, meeting_id, transcript, timestamp,
            audio_start_time, audio_end_time, duration, speaker,
            utterance_id, revision, latest_event_id, schema_version, session_id,
            event_kind, is_stable, sequence_id, start_ms, end_ms, audio_source,
            speaker_id, speaker_local_label, speaker_display_name,
            speaker_confidence, speaker_status, language,
            asr_provider, asr_model, asr_confidence, asr_latency_ms,
            diarization_provider, diarization_model, diarization_model_revision,
            diarization_revision, diarization_window_id,
            diarization_window_start_frame, diarization_window_end_frame,
            diarization_status, diarization_latency_ms,
            replaces_event_id, provider_event_id, trace_id,
            event_created_at, event_updated_at
        ) VALUES (
            ?, ?, ?, ?,
            ?, ?, ?, ?,
            ?, ?, ?, ?, ?,
            ?, ?, ?, ?, ?, ?,
            ?, ?, ?,
            ?, ?, ?,
            ?, ?, ?, ?,
            ?, ?, ?, ?, ?, ?, ?, ?, ?,
            ?, ?, ?,
            ?, ?
        )
        "#,
    )
    .bind(row_id)
    .bind(&event.meeting_id)
    .bind(&event.text)
    .bind(&event.timestamp)
    .bind(event.audio_start_time)
    .bind(event.audio_end_time)
    .bind(event.duration)
    .bind(&event.audio_source)
    .bind(&event.utterance_id)
    .bind(event.revision)
    .bind(&event.event_id)
    .bind(event.schema_version)
    .bind(&event.session_id)
    .bind(&event.event_kind)
    .bind(event.is_stable)
    .bind(event.sequence_id)
    .bind(event.start_ms)
    .bind(event.end_ms)
    .bind(&event.audio_source)
    .bind(&event.speaker_id)
    .bind(&event.speaker_local_label)
    .bind(&event.speaker_display_name)
    .bind(event.speaker_confidence)
    .bind(&event.speaker_status)
    .bind(&event.language)
    .bind(&event.asr_provider)
    .bind(&event.asr_model)
    .bind(event.asr_confidence)
    .bind(event.asr_latency_ms)
    .bind(&event.diarization_provider)
    .bind(&event.diarization_model)
    .bind(&event.diarization_model_revision)
    .bind(event.diarization_revision)
    .bind(&event.diarization_window_id)
    .bind(event.diarization_window_start_frame)
    .bind(event.diarization_window_end_frame)
    .bind(&event.diarization_status)
    .bind(event.diarization_latency_ms)
    .bind(&event.replaces_event_id)
    .bind(&event.provider_event_id)
    .bind(&event.trace_id)
    .bind(event.created_at)
    .bind(now)
    .execute(&mut **transaction)
    .await?;

    Ok(())
}

async fn update_materialized_transcript(
    transaction: &mut Transaction<'_, Sqlite>,
    event: &StoredTranscriptEvent,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE transcripts
        SET transcript = ?,
            timestamp = ?,
            audio_start_time = ?,
            audio_end_time = ?,
            duration = ?,
            speaker = ?,
            revision = ?,
            latest_event_id = ?,
            schema_version = ?,
            session_id = ?,
            event_kind = ?,
            is_stable = ?,
            sequence_id = ?,
            start_ms = ?,
            end_ms = ?,
            audio_source = ?,
            speaker_id = ?,
            speaker_local_label = ?,
            speaker_display_name = ?,
            speaker_confidence = ?,
            speaker_status = ?,
            language = ?,
            asr_provider = ?,
            asr_model = ?,
            asr_confidence = ?,
            asr_latency_ms = ?,
            diarization_provider = ?,
            diarization_model = ?,
            diarization_model_revision = ?,
            diarization_revision = ?,
            diarization_window_id = ?,
            diarization_window_start_frame = ?,
            diarization_window_end_frame = ?,
            diarization_status = ?,
            diarization_latency_ms = ?,
            replaces_event_id = ?,
            provider_event_id = ?,
            trace_id = ?,
            event_created_at = ?,
            event_updated_at = ?
        WHERE meeting_id = ? AND utterance_id = ?
        "#,
    )
    .bind(&event.text)
    .bind(&event.timestamp)
    .bind(event.audio_start_time)
    .bind(event.audio_end_time)
    .bind(event.duration)
    .bind(&event.audio_source)
    .bind(event.revision)
    .bind(&event.event_id)
    .bind(event.schema_version)
    .bind(&event.session_id)
    .bind(&event.event_kind)
    .bind(event.is_stable)
    .bind(event.sequence_id)
    .bind(event.start_ms)
    .bind(event.end_ms)
    .bind(&event.audio_source)
    .bind(&event.speaker_id)
    .bind(&event.speaker_local_label)
    .bind(&event.speaker_display_name)
    .bind(event.speaker_confidence)
    .bind(&event.speaker_status)
    .bind(&event.language)
    .bind(&event.asr_provider)
    .bind(&event.asr_model)
    .bind(event.asr_confidence)
    .bind(event.asr_latency_ms)
    .bind(&event.diarization_provider)
    .bind(&event.diarization_model)
    .bind(&event.diarization_model_revision)
    .bind(event.diarization_revision)
    .bind(&event.diarization_window_id)
    .bind(event.diarization_window_start_frame)
    .bind(event.diarization_window_end_frame)
    .bind(&event.diarization_status)
    .bind(event.diarization_latency_ms)
    .bind(&event.replaces_event_id)
    .bind(&event.provider_event_id)
    .bind(&event.trace_id)
    .bind(event.created_at)
    .bind(Utc::now())
    .bind(&event.meeting_id)
    .bind(&event.utterance_id)
    .execute(&mut **transaction)
    .await?;

    Ok(())
}

fn validate_event(event: &StoredTranscriptEvent) -> Result<(), TranscriptEventStoreError> {
    for (name, value) in [
        ("event_id", event.event_id.as_str()),
        ("meeting_id", event.meeting_id.as_str()),
        ("utterance_id", event.utterance_id.as_str()),
        ("event_kind", event.event_kind.as_str()),
        ("timestamp", event.timestamp.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(TranscriptEventStoreError::InvalidInput(format!(
                "{name} cannot be empty"
            )));
        }
    }

    if event.revision < 0 {
        return Err(TranscriptEventStoreError::InvalidInput(
            "revision must be non-negative".to_string(),
        ));
    }

    if event.schema_version < 0 {
        return Err(TranscriptEventStoreError::InvalidInput(
            "schema_version must be non-negative".to_string(),
        ));
    }

    if let Some(start_ms) = event.start_ms {
        if start_ms < 0 {
            return Err(TranscriptEventStoreError::InvalidInput(
                "start_ms must be non-negative".to_string(),
            ));
        }
    }

    if let Some(end_ms) = event.end_ms {
        if end_ms < 0 {
            return Err(TranscriptEventStoreError::InvalidInput(
                "end_ms must be non-negative".to_string(),
            ));
        }
        if let Some(start_ms) = event.start_ms {
            if end_ms < start_ms {
                return Err(TranscriptEventStoreError::InvalidInput(
                    "end_ms cannot be earlier than start_ms".to_string(),
                ));
            }
        }
    }

    if let Some(start) = event.audio_start_time {
        if !start.is_finite() || start < 0.0 {
            return Err(TranscriptEventStoreError::InvalidInput(
                "audio_start_time must be finite and non-negative".to_string(),
            ));
        }
    }

    if let Some(end) = event.audio_end_time {
        if !end.is_finite() || end < 0.0 {
            return Err(TranscriptEventStoreError::InvalidInput(
                "audio_end_time must be finite and non-negative".to_string(),
            ));
        }
        if let Some(start) = event.audio_start_time {
            if end < start {
                return Err(TranscriptEventStoreError::InvalidInput(
                    "audio_end_time cannot be earlier than audio_start_time".to_string(),
                ));
            }
        }
    }

    if let Some(duration) = event.duration {
        if !duration.is_finite() || duration < 0.0 {
            return Err(TranscriptEventStoreError::InvalidInput(
                "duration must be finite and non-negative".to_string(),
            ));
        }
    }

    let has_diarization = event.diarization_provider.is_some()
        || event.diarization_model.is_some()
        || event.diarization_model_revision.is_some()
        || event.diarization_revision.is_some()
        || event.diarization_window_id.is_some()
        || event.diarization_window_start_frame.is_some()
        || event.diarization_window_end_frame.is_some()
        || event.diarization_status.is_some()
        || event.diarization_latency_ms.is_some();
    if has_diarization {
        for (name, value) in [
            (
                "diarization_provider",
                event.diarization_provider.as_deref(),
            ),
            ("diarization_status", event.diarization_status.as_deref()),
        ] {
            if value.map_or(true, |value| value.trim().is_empty()) {
                return Err(TranscriptEventStoreError::InvalidInput(format!(
                    "{name} is required when diarization metadata is present"
                )));
            }
        }
        if event.diarization_revision.is_none() {
            return Err(TranscriptEventStoreError::InvalidInput(
                "diarization_revision is required when diarization metadata is present".to_string(),
            ));
        }
        for (name, value) in [
            ("diarization_model", event.diarization_model.as_deref()),
            (
                "diarization_model_revision",
                event.diarization_model_revision.as_deref(),
            ),
            (
                "diarization_window_id",
                event.diarization_window_id.as_deref(),
            ),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(TranscriptEventStoreError::InvalidInput(format!(
                    "{name} cannot be empty when present"
                )));
            }
        }
    }

    if event
        .diarization_revision
        .is_some_and(|revision| revision < 0)
    {
        return Err(TranscriptEventStoreError::InvalidInput(
            "diarization_revision must be non-negative".to_string(),
        ));
    }
    if event
        .diarization_latency_ms
        .is_some_and(|latency_ms| latency_ms < 0)
    {
        return Err(TranscriptEventStoreError::InvalidInput(
            "diarization_latency_ms must be non-negative".to_string(),
        ));
    }
    match (
        event.diarization_window_start_frame,
        event.diarization_window_end_frame,
    ) {
        (Some(start), Some(end)) if start < 0 || end <= start => {
            return Err(TranscriptEventStoreError::InvalidInput(
                "diarization window must have non-negative start and end after start".to_string(),
            ));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(TranscriptEventStoreError::InvalidInput(
                "diarization window start/end frames must be provided together".to_string(),
            ));
        }
        _ => {}
    }

    Ok(())
}

fn revision_payload_matches(existing: &UtteranceRevision, event: &StoredTranscriptEvent) -> bool {
    existing.event_id == event.event_id
        && existing.meeting_id == event.meeting_id
        && existing.schema_version == event.schema_version
        && existing.session_id == event.session_id
        && existing.utterance_id == event.utterance_id
        && existing.revision == event.revision
        && existing.event_kind == event.event_kind
        && existing.is_stable == event.is_stable
        && existing.transcript == event.text
        && existing.timestamp == event.timestamp
        && existing.sequence_id == event.sequence_id
        && existing.start_ms == event.start_ms
        && existing.end_ms == event.end_ms
        && option_f64_eq(existing.audio_start_time, event.audio_start_time)
        && option_f64_eq(existing.audio_end_time, event.audio_end_time)
        && option_f64_eq(existing.duration, event.duration)
        && existing.audio_source == event.audio_source
        && existing.speaker_id == event.speaker_id
        && existing.speaker_local_label == event.speaker_local_label
        && existing.speaker_display_name == event.speaker_display_name
        && option_f64_eq(existing.speaker_confidence, event.speaker_confidence)
        && existing.speaker_status == event.speaker_status
        && existing.language == event.language
        && existing.asr_provider == event.asr_provider
        && existing.asr_model == event.asr_model
        && option_f64_eq(existing.asr_confidence, event.asr_confidence)
        && existing.asr_latency_ms == event.asr_latency_ms
        && existing.diarization_provider == event.diarization_provider
        && existing.diarization_model == event.diarization_model
        && existing.diarization_model_revision == event.diarization_model_revision
        && existing.diarization_revision == event.diarization_revision
        && existing.diarization_window_id == event.diarization_window_id
        && existing.diarization_window_start_frame == event.diarization_window_start_frame
        && existing.diarization_window_end_frame == event.diarization_window_end_frame
        && existing.diarization_status == event.diarization_status
        && existing.diarization_latency_ms == event.diarization_latency_ms
        && existing.replaces_event_id == event.replaces_event_id
        && existing.provider_event_id == event.provider_event_id
        && existing.trace_id == event.trace_id
}

fn option_f64_eq(left: Option<f64>, right: Option<f64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => (left - right).abs() <= 1e-9,
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("create in-memory database");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind("meeting-1")
            .bind("Synthetic test meeting")
            .bind(Utc::now())
            .bind(Utc::now())
            .execute(&pool)
            .await
            .expect("insert meeting");
        pool
    }

    fn event(revision: i64, text: &str, stable: bool) -> StoredTranscriptEvent {
        StoredTranscriptEvent {
            event_id: format!("event-{revision}"),
            meeting_id: "meeting-1".to_string(),
            schema_version: 1,
            session_id: Some("session-1".to_string()),
            utterance_id: "utterance-1".to_string(),
            revision,
            event_kind: if stable { "final" } else { "partial" }.to_string(),
            is_stable: stable,
            text: text.to_string(),
            timestamp: "00:00:01".to_string(),
            sequence_id: Some(1),
            start_ms: Some(1_000),
            end_ms: Some(2_000),
            audio_start_time: Some(1.0),
            audio_end_time: Some(2.0),
            duration: Some(1.0),
            audio_source: Some("system".to_string()),
            speaker_id: Some("speaker-1".to_string()),
            speaker_local_label: Some("Speaker 1".to_string()),
            speaker_display_name: Some("测试发言人".to_string()),
            speaker_confidence: Some(0.91),
            speaker_status: Some("resolved".to_string()),
            language: Some("zh".to_string()),
            asr_provider: Some("synthetic".to_string()),
            asr_model: Some("test-model".to_string()),
            asr_confidence: Some(0.95),
            asr_latency_ms: Some(250),
            diarization_provider: Some("moss-worker".to_string()),
            diarization_model: Some("MOSS-Transcribe-Diarize".to_string()),
            diarization_model_revision: Some("fixture-revision".to_string()),
            diarization_revision: Some(100 + revision),
            diarization_window_id: Some("window-1".to_string()),
            diarization_window_start_frame: Some(48_000),
            diarization_window_end_frame: Some(96_000),
            diarization_status: Some("provisional".to_string()),
            diarization_latency_ms: Some(1_500 + revision),
            replaces_event_id: (revision > 1).then(|| format!("event-{}", revision - 1)),
            provider_event_id: Some(format!("provider-{revision}")),
            trace_id: Some("trace-1".to_string()),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn upsert_is_idempotent_and_never_downgrades_latest_revision() {
        let pool = test_pool().await;

        let first = event(0, "初稿", false);
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &first)
                .await
                .expect("apply revision 0"),
            TranscriptUpsertOutcome::Applied {
                previous_revision: None
            }
        );
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &first)
                .await
                .expect("retry revision 0"),
            TranscriptUpsertOutcome::Duplicate { revision: 0 }
        );

        let second = event(1, "中间文本", false);
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &second)
                .await
                .expect("apply revision 1"),
            TranscriptUpsertOutcome::Applied {
                previous_revision: Some(0)
            }
        );

        let third = event(2, "最终文本", true);
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &third)
                .await
                .expect("apply revision 2"),
            TranscriptUpsertOutcome::Applied {
                previous_revision: Some(1)
            }
        );

        let delayed = event(1, "中间文本", false);
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &delayed)
                .await
                .expect("retry older revision 1"),
            TranscriptUpsertOutcome::Duplicate { revision: 1 }
        );

        let latest = TranscriptEventsRepository::list_latest(&pool, "meeting-1")
            .await
            .expect("read latest");
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].revision, 2);
        assert_eq!(latest[0].transcript, "最终文本");
        assert_eq!(latest[0].schema_version, 1);
        assert_eq!(latest[0].start_ms, Some(1_000));
        assert_eq!(latest[0].end_ms, Some(2_000));
        assert_eq!(latest[0].audio_source.as_deref(), Some("system"));
        assert_eq!(latest[0].speaker_id.as_deref(), Some("speaker-1"));
        assert_eq!(latest[0].asr_provider.as_deref(), Some("synthetic"));
        assert_eq!(
            latest[0].diarization_provider.as_deref(),
            Some("moss-worker")
        );
        assert_eq!(
            latest[0].diarization_model.as_deref(),
            Some("MOSS-Transcribe-Diarize")
        );
        assert_eq!(
            latest[0].diarization_model_revision.as_deref(),
            Some("fixture-revision")
        );
        assert_eq!(latest[0].diarization_revision, Some(102));
        assert_eq!(latest[0].diarization_window_id.as_deref(), Some("window-1"));
        assert_eq!(latest[0].diarization_window_start_frame, Some(48_000));
        assert_eq!(latest[0].diarization_window_end_frame, Some(96_000));
        assert_eq!(latest[0].diarization_status.as_deref(), Some("provisional"));
        assert_eq!(latest[0].diarization_latency_ms, Some(1_502));
        assert_eq!(
            latest[0].speaker_display_name.as_deref(),
            Some("测试发言人")
        );

        let revisions =
            TranscriptEventsRepository::list_revisions(&pool, "meeting-1", "utterance-1")
                .await
                .expect("read revision history");
        assert_eq!(
            revisions.iter().map(|row| row.revision).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(revisions[2].diarization_revision, Some(102));
        assert_eq!(revisions[2].asr_model.as_deref(), Some("test-model"));
    }

    #[tokio::test]
    async fn same_event_id_with_different_payload_is_rejected() {
        let pool = test_pool().await;
        TranscriptEventsRepository::upsert_revision(&pool, &event(0, "版本 A", false))
            .await
            .expect("apply revision");

        let error = TranscriptEventsRepository::upsert_revision(&pool, &event(0, "版本 B", false))
            .await
            .expect_err("conflicting payload must fail");

        assert!(matches!(
            error,
            TranscriptEventStoreError::EventIdConflict { .. }
        ));
    }

    #[tokio::test]
    async fn same_transcript_revision_cannot_change_diarization_provenance() {
        let pool = test_pool().await;
        let original = event(0, "版本 A", false);
        TranscriptEventsRepository::upsert_revision(&pool, &original)
            .await
            .expect("apply revision");

        let mut conflicting = original;
        conflicting.diarization_model_revision = Some("different-model-sha".to_string());
        let error = TranscriptEventsRepository::upsert_revision(&pool, &conflicting)
            .await
            .expect_err("immutable revision must reject changed diarization metadata");
        assert!(matches!(
            error,
            TranscriptEventStoreError::EventIdConflict { .. }
        ));
    }

    #[tokio::test]
    async fn same_revision_final_replaces_partial_and_late_partial_cannot_roll_back() {
        let pool = test_pool().await;
        let mut partial = event(0, "未完成", false);
        partial.event_id = "same-revision-partial".to_string();
        partial.created_at = "2026-09-02T00:00:01Z".parse().unwrap();
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &partial)
                .await
                .expect("store partial"),
            TranscriptUpsertOutcome::Applied {
                previous_revision: None
            }
        );

        let mut final_event = event(0, "最终文本", true);
        final_event.event_id = "same-revision-final".to_string();
        final_event.created_at = "2026-09-02T00:00:02Z".parse().unwrap();
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &final_event)
                .await
                .expect("same revision final outranks partial"),
            TranscriptUpsertOutcome::Applied {
                previous_revision: Some(0)
            }
        );

        let mut delayed_partial = event(0, "迟到的未完成稿", false);
        delayed_partial.event_id = "same-revision-delayed-partial".to_string();
        delayed_partial.created_at = "2026-09-02T00:00:03Z".parse().unwrap();
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &delayed_partial)
                .await
                .expect("audit delayed partial"),
            TranscriptUpsertOutcome::Stale {
                revision: 0,
                current_revision: 0,
            }
        );

        let latest = TranscriptEventsRepository::list_latest(&pool, "meeting-1")
            .await
            .expect("load deterministic latest");
        assert_eq!(
            latest[0].latest_event_id.as_deref(),
            Some("same-revision-final")
        );
        assert_eq!(latest[0].transcript, "最终文本");

        let history = TranscriptEventsRepository::list_revisions(&pool, "meeting-1", "utterance-1")
            .await
            .expect("keep every immutable delivery");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].event_id, "same-revision-partial");
        assert_eq!(history[1].event_id, "same-revision-delayed-partial");
        assert_eq!(history[2].event_id, "same-revision-final");
    }

    #[tokio::test]
    async fn equal_priority_same_revision_events_converge_independent_of_delivery_order() {
        let pool_forward = test_pool().await;
        let pool_reverse = test_pool().await;

        let mut event_a = event(0, "候选 A", true);
        event_a.event_id = "same-priority-a".to_string();
        event_a.created_at = "2026-09-02T00:00:01Z".parse().unwrap();

        let mut event_b = event(0, "候选 B", true);
        event_b.event_id = "same-priority-b".to_string();
        event_b.created_at = event_a.created_at;

        TranscriptEventsRepository::upsert_revision(&pool_forward, &event_a)
            .await
            .expect("store A first");
        TranscriptEventsRepository::upsert_revision(&pool_forward, &event_b)
            .await
            .expect("store B second");

        TranscriptEventsRepository::upsert_revision(&pool_reverse, &event_b)
            .await
            .expect("store B first");
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool_reverse, &event_a)
                .await
                .expect("audit late A"),
            TranscriptUpsertOutcome::Stale {
                revision: 0,
                current_revision: 0,
            }
        );

        for pool in [&pool_forward, &pool_reverse] {
            let latest = TranscriptEventsRepository::list_latest(pool, "meeting-1")
                .await
                .expect("load converged latest");
            assert_eq!(
                latest[0].latest_event_id.as_deref(),
                Some("same-priority-b")
            );
            assert_eq!(latest[0].transcript, "候选 B");
        }
    }

    #[test]
    fn incomplete_or_invalid_diarization_windows_fail_closed() {
        let mut incomplete = event(0, "版本 A", false);
        incomplete.diarization_window_end_frame = None;
        assert!(matches!(
            validate_event(&incomplete),
            Err(TranscriptEventStoreError::InvalidInput(message))
                if message.contains("provided together")
        ));

        let mut reversed = event(0, "版本 A", false);
        reversed.diarization_window_end_frame = Some(47_999);
        assert!(matches!(
            validate_event(&reversed),
            Err(TranscriptEventStoreError::InvalidInput(message))
                if message.contains("end after start")
        ));
    }

    #[tokio::test]
    async fn delayed_unseen_revision_is_kept_in_history_without_downgrade() {
        let pool = test_pool().await;
        let mut revision_zero = event(0, "开始", false);
        revision_zero.event_id = "gap-event-0".to_string();
        revision_zero.utterance_id = "utterance-gap".to_string();
        TranscriptEventsRepository::upsert_revision(&pool, &revision_zero)
            .await
            .expect("apply revision 0");

        let mut revision_two = event(2, "完成", true);
        revision_two.event_id = "gap-event-2".to_string();
        revision_two.utterance_id = "utterance-gap".to_string();
        TranscriptEventsRepository::upsert_revision(&pool, &revision_two)
            .await
            .expect("apply revision 2");

        let mut delayed_revision_one = event(1, "迟到的中间稿", false);
        delayed_revision_one.event_id = "gap-event-1".to_string();
        delayed_revision_one.utterance_id = "utterance-gap".to_string();
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &delayed_revision_one)
                .await
                .expect("store delayed revision 1"),
            TranscriptUpsertOutcome::Stale {
                revision: 1,
                current_revision: 2
            }
        );

        let history =
            TranscriptEventsRepository::list_revisions(&pool, "meeting-1", "utterance-gap")
                .await
                .expect("read gap history");
        assert_eq!(
            history.iter().map(|row| row.revision).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[tokio::test]
    async fn retraction_hides_materialized_text_and_blocks_stale_resurrection() {
        let pool = test_pool().await;
        TranscriptEventsRepository::upsert_revision(&pool, &event(1, "可见文本", true))
            .await
            .expect("apply visible revision");

        let mut retraction = event(3, "", true);
        retraction.event_id = "event-retraction".to_string();
        retraction.event_kind = "retraction".to_string();
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &retraction)
                .await
                .expect("apply retraction"),
            TranscriptUpsertOutcome::Applied {
                previous_revision: Some(1)
            }
        );
        assert!(TranscriptEventsRepository::list_latest(&pool, "meeting-1")
            .await
            .expect("read hidden materialization")
            .is_empty());

        let mut delayed = event(2, "不得复活", true);
        delayed.event_id = "event-delayed".to_string();
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &delayed)
                .await
                .expect("audit delayed revision"),
            TranscriptUpsertOutcome::Stale {
                revision: 2,
                current_revision: 3
            }
        );
        assert!(TranscriptEventsRepository::list_latest(&pool, "meeting-1")
            .await
            .expect("stale event stays hidden")
            .is_empty());

        let mut restored = event(4, "显式恢复", true);
        restored.event_id = "event-restored".to_string();
        restored.event_kind = "correction".to_string();
        assert_eq!(
            TranscriptEventsRepository::upsert_revision(&pool, &restored)
                .await
                .expect("apply newer restoration"),
            TranscriptUpsertOutcome::Applied {
                previous_revision: Some(3)
            }
        );
        let latest = TranscriptEventsRepository::list_latest(&pool, "meeting-1")
            .await
            .expect("read restored materialization");
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].revision, 4);
        assert_eq!(latest[0].transcript, "显式恢复");
    }
}
