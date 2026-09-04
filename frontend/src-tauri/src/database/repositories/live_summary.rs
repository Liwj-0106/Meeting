use crate::audio::transcription::event::{
    AudioSource, SpeakerMetadata, SpeakerStatus, TranscriptEvent, TranscriptEventKind,
};
use crate::database::models::UtteranceRevision;
use crate::summary::live::{
    source_text_hash, LiveSummaryContractError, LiveSummaryItem, LiveSummaryRevision,
    LiveSummaryRevisionType, LiveSummaryScope, SummaryEvidence, SummaryItemKind, SummaryItemStatus,
};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;

pub struct LiveSummaryRepository;

const DEFAULT_LIVE_SUMMARY_TEMPLATE_ID: &str = "standard_meeting";
const MAX_LIVE_SUMMARY_PROMPT_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSummaryPreferencesView {
    pub template_id: String,
    pub has_custom_prompt: bool,
}

/// Write-only custom prompt. It intentionally implements neither `Debug`,
/// `Display` nor `Serialize`, so a runtime caller must explicitly opt in to
/// reading its bytes and cannot return it over IPC by accident.
pub(crate) struct LiveSummaryPrompt(String);

impl LiveSummaryPrompt {
    pub(crate) fn expose_secret(&self) -> &str {
        &self.0
    }
}

pub(crate) struct LiveSummaryRuntimePreferences {
    pub(crate) template_id: String,
    pub(crate) custom_prompt: Option<LiveSummaryPrompt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveSummaryRecoverySnapshot {
    pub latest_revision: Option<LiveSummaryRevision>,
    pub current_transcript_cursor: u64,
    pub transcript_heads: Vec<TranscriptEvent>,
}

#[derive(Debug, Error)]
pub enum LiveSummaryStoreError {
    #[error(transparent)]
    Contract(#[from] LiveSummaryContractError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("summary numeric field exceeds SQLite's signed integer range")]
    IntegerOutOfRange,
    #[error("summary evidence source is missing or is not a stable final/correction event")]
    EvidenceSourceMissing,
    #[error("summary evidence text hash does not match the immutable transcript revision")]
    EvidenceHashMismatch,
    #[error("stored live summary row is invalid: {0}")]
    InvalidStoredState(&'static str),
}

#[derive(Debug, FromRow)]
struct RevisionRow {
    summary_revision_id: String,
    meeting_id: String,
    revision: i64,
    generation: i64,
    transcript_cursor: i64,
    snapshot_hash: String,
    revision_type: String,
    provider: String,
    model: Option<String>,
    created_at: String,
}

#[derive(Debug, FromRow)]
struct ItemRow {
    item_id: String,
    kind: String,
    title: String,
    body: String,
    owner: Option<String>,
    due_at: Option<String>,
    status: String,
}

#[derive(Debug, FromRow)]
struct EvidenceRow {
    meeting_id: String,
    utterance_id: String,
    source_revision: i64,
    source_event_id: String,
    source_text_hash: String,
}

impl LiveSummaryRepository {
    pub async fn load_preferences_view(
        pool: &SqlitePool,
    ) -> Result<LiveSummaryPreferencesView, LiveSummaryStoreError> {
        let row = sqlx::query_as::<_, (String, bool)>(
            r#"
            SELECT template_id, custom_prompt IS NOT NULL
            FROM live_summary_preferences
            WHERE id = 1
            "#,
        )
        .fetch_optional(pool)
        .await?;
        let (template_id, has_custom_prompt) =
            row.unwrap_or_else(|| (DEFAULT_LIVE_SUMMARY_TEMPLATE_ID.to_string(), false));
        Ok(LiveSummaryPreferencesView {
            template_id,
            has_custom_prompt,
        })
    }

    pub(crate) async fn load_runtime_preferences(
        pool: &SqlitePool,
    ) -> Result<LiveSummaryRuntimePreferences, LiveSummaryStoreError> {
        let row = sqlx::query_as::<_, (String, Option<String>)>(
            r#"
            SELECT template_id, custom_prompt
            FROM live_summary_preferences
            WHERE id = 1
            "#,
        )
        .fetch_optional(pool)
        .await?;
        let (template_id, custom_prompt) =
            row.unwrap_or_else(|| (DEFAULT_LIVE_SUMMARY_TEMPLATE_ID.to_string(), None));
        Ok(LiveSummaryRuntimePreferences {
            template_id,
            custom_prompt: custom_prompt.map(LiveSummaryPrompt),
        })
    }

    /// Update the default template and optionally replace the private prompt.
    /// `None` retains an existing prompt so the renderer never needs to read
    /// it merely to change templates. Deletion is an explicit separate call.
    pub async fn save_preferences(
        pool: &SqlitePool,
        template_id: &str,
        replacement_prompt: Option<&str>,
    ) -> Result<LiveSummaryPreferencesView, LiveSummaryStoreError> {
        crate::summary::templates::validate_template_id(template_id).map_err(|_| {
            LiveSummaryStoreError::InvalidStoredState("invalid template identifier")
        })?;
        let replacement_prompt = replacement_prompt
            .map(validate_live_summary_prompt)
            .transpose()?;
        let updated_at = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO live_summary_preferences (id, template_id, custom_prompt, updated_at)
            VALUES (1, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                template_id = excluded.template_id,
                custom_prompt = CASE
                    WHEN ? THEN excluded.custom_prompt
                    ELSE live_summary_preferences.custom_prompt
                END,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(template_id)
        .bind(replacement_prompt.as_deref())
        .bind(updated_at)
        .bind(replacement_prompt.is_some())
        .execute(pool)
        .await?;
        Self::load_preferences_view(pool).await
    }

    pub async fn delete_custom_prompt(
        pool: &SqlitePool,
    ) -> Result<LiveSummaryPreferencesView, LiveSummaryStoreError> {
        sqlx::query(
            r#"
            UPDATE live_summary_preferences
            SET custom_prompt = NULL, updated_at = ?
            WHERE id = 1
            "#,
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
        Self::load_preferences_view(pool).await
    }

    /// Persist one complete revision atomically. The immutable revision row is
    /// inserted first, then every item and exact transcript binding. Any
    /// failure explicitly rolls the whole transaction back, including the
    /// latest-projection trigger fired by the revision insert.
    pub async fn persist_revision(
        pool: &SqlitePool,
        revision: &LiveSummaryRevision,
    ) -> Result<(), LiveSummaryStoreError> {
        revision.validate()?;
        let mut transaction = pool.begin().await?;
        match persist_in_transaction(&mut transaction, revision).await {
            Ok(()) => {
                transaction.commit().await?;
                Ok(())
            }
            Err(error) => {
                transaction.rollback().await?;
                Err(error)
            }
        }
    }

    /// Load the materialized latest complete revision with its ordered items
    /// and evidence. `transcript_cursor`, `revision`, and `generation` are part
    /// of the returned snapshot and seed `LiveSummaryActor::recover`.
    pub async fn load_latest_revision(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<LiveSummaryRevision>, LiveSummaryStoreError> {
        let row = sqlx::query_as::<_, RevisionRow>(
            r#"
            SELECT
                revisions.summary_revision_id,
                revisions.meeting_id,
                revisions.revision,
                revisions.generation,
                revisions.transcript_cursor,
                revisions.snapshot_hash,
                revisions.revision_type,
                revisions.provider,
                revisions.model,
                revisions.created_at
            FROM live_summary_sessions AS sessions
            JOIN live_summary_revisions AS revisions
              ON revisions.summary_revision_id = sessions.latest_revision_id
            WHERE sessions.meeting_id = ?
            "#,
        )
        .bind(meeting_id)
        .fetch_optional(pool)
        .await?;

        match row {
            Some(row) => Ok(Some(load_revision_children(pool, row).await?)),
            None => Ok(None),
        }
    }

    /// Load the durable state needed to restart a live-summary actor. The
    /// cursor counts every accepted stable source revision while
    /// `transcript_heads` contains only the newest revision per utterance.
    /// Keeping retraction heads prevents an older final from being restored.
    pub async fn load_recovery_snapshot(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<LiveSummaryRecoverySnapshot, LiveSummaryStoreError> {
        let latest_revision = Self::load_latest_revision(pool, meeting_id).await?;
        let cursor = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM utterance_revisions
            WHERE meeting_id = ?
              AND is_stable = 1
              AND event_kind IN ('final', 'correction', 'retraction')
            "#,
        )
        .bind(meeting_id)
        .fetch_one(pool)
        .await?;

        let rows = sqlx::query_as::<_, UtteranceRevision>(
            r#"
            WITH ranked_heads AS (
                SELECT
                    source.*,
                    ROW_NUMBER() OVER (
                        PARTITION BY source.utterance_id
                        ORDER BY
                            source.revision DESC,
                            source.is_stable DESC,
                            CASE lower(source.event_kind)
                                WHEN 'partial' THEN 1
                                WHEN 'final' THEN 2
                                WHEN 'correction' THEN 3
                                WHEN 'speaker_update' THEN 4
                                WHEN 'language_update' THEN 4
                                WHEN 'retraction' THEN 5
                                ELSE 0
                            END DESC,
                            source.created_at DESC,
                            source.event_id DESC
                    ) AS head_rank
                FROM utterance_revisions
                AS source
                WHERE source.meeting_id = ?
                  AND source.is_stable = 1
                  AND source.event_kind IN ('final', 'correction', 'retraction')
            )
            SELECT
                source.event_id, source.meeting_id, source.schema_version,
                source.session_id, source.utterance_id, source.revision,
                source.event_kind, source.is_stable, source.transcript,
                source.timestamp, source.sequence_id, source.start_ms,
                source.end_ms, source.audio_start_time, source.audio_end_time,
                source.duration, source.audio_source, source.speaker_id,
                source.speaker_local_label, source.speaker_display_name,
                source.speaker_confidence, source.speaker_status, source.language,
                source.asr_provider, source.asr_model, source.asr_confidence,
                source.asr_latency_ms, source.diarization_provider,
                source.diarization_model, source.diarization_model_revision,
                source.diarization_revision, source.diarization_window_id,
                source.diarization_window_start_frame,
                source.diarization_window_end_frame, source.diarization_status,
                source.diarization_latency_ms, source.replaces_event_id,
                source.provider_event_id, source.trace_id, source.created_at
            FROM ranked_heads AS source
            WHERE source.head_rank = 1
            ORDER BY
                COALESCE(source.start_ms, 9223372036854775807),
                source.utterance_id
            "#,
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await?;

        let transcript_heads = rows
            .into_iter()
            .map(transcript_head_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LiveSummaryRecoverySnapshot {
            latest_revision,
            current_transcript_cursor: unsigned_integer(cursor)?,
            transcript_heads,
        })
    }
}

fn validate_live_summary_prompt(value: &str) -> Result<String, LiveSummaryStoreError> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > MAX_LIVE_SUMMARY_PROMPT_CHARS
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(LiveSummaryStoreError::InvalidStoredState(
            "custom summary prompt is empty or outside its safety limits",
        ));
    }
    Ok(value.to_string())
}

fn transcript_head_from_row(
    row: UtteranceRevision,
) -> Result<TranscriptEvent, LiveSummaryStoreError> {
    let schema_version = u16::try_from(row.schema_version)
        .map_err(|_| LiveSummaryStoreError::InvalidStoredState("invalid transcript schema"))?;
    let revision = unsigned_integer(row.revision)?;
    let start_ms = stored_time_ms(row.start_ms, row.audio_start_time)?;
    let end_ms = stored_time_ms(row.end_ms, row.audio_end_time)?.max(start_ms);
    let sequence_id = row.sequence_id.map(unsigned_integer).transpose()?;
    let speaker = row.speaker_id.map(|speaker_id| SpeakerMetadata {
        speaker_id,
        local_label: row.speaker_local_label,
        display_name: row.speaker_display_name,
        confidence: row.speaker_confidence.map(|value| value as f32),
        status: match row.speaker_status.as_deref() {
            Some("provisional") => SpeakerStatus::Provisional,
            Some("resolved") => SpeakerStatus::Resolved,
            Some("user_confirmed") => SpeakerStatus::UserConfirmed,
            Some("renamed") => SpeakerStatus::Renamed,
            Some("merged") => SpeakerStatus::Merged,
            Some(value) if value != "unresolved" => SpeakerStatus::Unknown(value.to_string()),
            _ => SpeakerStatus::Unresolved,
        },
    });

    Ok(TranscriptEvent {
        schema_version,
        event_id: row.event_id,
        meeting_id: Some(row.meeting_id),
        session_id: row.session_id,
        utterance_id: row.utterance_id,
        revision,
        event_kind: match row.event_kind.as_str() {
            "final" => TranscriptEventKind::Final,
            "correction" => TranscriptEventKind::Correction,
            "retraction" => TranscriptEventKind::Retraction,
            value => TranscriptEventKind::Unknown(value.to_string()),
        },
        is_stable: row.is_stable,
        start_ms,
        end_ms,
        text: row.transcript,
        language: row.language,
        audio_source: row
            .audio_source
            .as_deref()
            .map(AudioSource::from_legacy_label)
            .unwrap_or_default(),
        speaker,
        asr: None,
        diarization: None,
        replaces_event_id: row.replaces_event_id,
        provider_event_id: row.provider_event_id,
        created_at: row.created_at.to_rfc3339(),
        trace_id: row.trace_id,
        sequence_id,
    })
}

fn stored_time_ms(
    milliseconds: Option<i64>,
    seconds: Option<f64>,
) -> Result<u64, LiveSummaryStoreError> {
    if let Some(milliseconds) = milliseconds {
        return unsigned_integer(milliseconds);
    }
    let seconds = seconds.unwrap_or(0.0);
    if !seconds.is_finite() || seconds < 0.0 || seconds > u64::MAX as f64 / 1_000.0 {
        return Err(LiveSummaryStoreError::InvalidStoredState(
            "invalid transcript timestamp",
        ));
    }
    Ok((seconds * 1_000.0).round() as u64)
}

async fn persist_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    revision: &LiveSummaryRevision,
) -> Result<(), LiveSummaryStoreError> {
    let meeting_id =
        revision
            .scope
            .meeting_id()
            .ok_or(LiveSummaryStoreError::InvalidStoredState(
                "recording-session summary revisions cannot be persisted before binding",
            ))?;
    let revision_number = sqlite_integer(revision.revision)?;
    let generation = sqlite_integer(revision.generation)?;
    let transcript_cursor = sqlite_integer(revision.transcript_cursor)?;

    sqlx::query(
        r#"
        INSERT INTO live_summary_revisions (
            summary_revision_id,
            meeting_id,
            revision,
            generation,
            transcript_cursor,
            snapshot_hash,
            revision_type,
            provider,
            model,
            created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&revision.summary_revision_id)
    .bind(meeting_id)
    .bind(revision_number)
    .bind(generation)
    .bind(transcript_cursor)
    .bind(&revision.snapshot_hash)
    .bind(revision.revision_type.as_str())
    .bind(&revision.provider)
    .bind(&revision.model)
    .bind(&revision.created_at)
    .execute(&mut **transaction)
    .await?;

    for (position, item) in revision.items.iter().enumerate() {
        sqlx::query(
            r#"
            INSERT INTO live_summary_items (
                summary_revision_id,
                item_id,
                position,
                kind,
                title,
                body,
                owner,
                due_at,
                status
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&revision.summary_revision_id)
        .bind(&item.item_id)
        .bind(i64::try_from(position).map_err(|_| LiveSummaryStoreError::IntegerOutOfRange)?)
        .bind(item.kind.as_str())
        .bind(&item.title)
        .bind(&item.body)
        .bind(&item.owner)
        .bind(&item.due_at)
        .bind(item.status.as_str())
        .execute(&mut **transaction)
        .await?;

        for (evidence_index, evidence) in item.evidence.iter().enumerate() {
            verify_evidence(transaction, meeting_id, evidence).await?;
            sqlx::query(
                r#"
                INSERT INTO live_summary_evidence (
                    summary_revision_id,
                    item_id,
                    evidence_index,
                    meeting_id,
                    utterance_id,
                    source_revision,
                    source_event_id,
                    source_text_hash
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(&revision.summary_revision_id)
            .bind(&item.item_id)
            .bind(
                i64::try_from(evidence_index)
                    .map_err(|_| LiveSummaryStoreError::IntegerOutOfRange)?,
            )
            .bind(
                evidence
                    .scope
                    .meeting_id()
                    .ok_or(LiveSummaryStoreError::InvalidStoredState(
                        "recording-session evidence cannot be persisted before binding",
                    ))?,
            )
            .bind(&evidence.utterance_id)
            .bind(sqlite_integer(evidence.source_revision)?)
            .bind(&evidence.source_event_id)
            .bind(&evidence.source_text_hash)
            .execute(&mut **transaction)
            .await?;
        }
    }
    Ok(())
}

async fn verify_evidence(
    transaction: &mut Transaction<'_, Sqlite>,
    revision_meeting_id: &str,
    evidence: &SummaryEvidence,
) -> Result<(), LiveSummaryStoreError> {
    if evidence.scope != LiveSummaryScope::meeting(revision_meeting_id) {
        return Err(LiveSummaryContractError::ScopeMismatch.into());
    }
    let transcript = sqlx::query_scalar::<_, String>(
        r#"
        SELECT transcript
        FROM utterance_revisions
        WHERE event_id = ?
          AND meeting_id = ?
          AND utterance_id = ?
          AND revision = ?
          AND is_stable = 1
          AND event_kind IN ('final', 'correction')
        "#,
    )
    .bind(&evidence.source_event_id)
    .bind(revision_meeting_id)
    .bind(&evidence.utterance_id)
    .bind(sqlite_integer(evidence.source_revision)?)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(LiveSummaryStoreError::EvidenceSourceMissing)?;

    if source_text_hash(&transcript) != evidence.source_text_hash {
        return Err(LiveSummaryStoreError::EvidenceHashMismatch);
    }
    Ok(())
}

async fn load_revision_children(
    pool: &SqlitePool,
    row: RevisionRow,
) -> Result<LiveSummaryRevision, LiveSummaryStoreError> {
    let item_rows = sqlx::query_as::<_, ItemRow>(
        r#"
        SELECT item_id, kind, title, body, owner, due_at, status
        FROM live_summary_items
        WHERE summary_revision_id = ?
        ORDER BY position ASC
        "#,
    )
    .bind(&row.summary_revision_id)
    .fetch_all(pool)
    .await?;

    let mut items = Vec::with_capacity(item_rows.len());
    for item_row in item_rows {
        let evidence_rows = sqlx::query_as::<_, EvidenceRow>(
            r#"
            SELECT
                meeting_id,
                utterance_id,
                source_revision,
                source_event_id,
                source_text_hash
            FROM live_summary_evidence
            WHERE summary_revision_id = ? AND item_id = ?
            ORDER BY evidence_index ASC
            "#,
        )
        .bind(&row.summary_revision_id)
        .bind(&item_row.item_id)
        .fetch_all(pool)
        .await?;

        let evidence = evidence_rows
            .into_iter()
            .map(|value| {
                Ok(SummaryEvidence {
                    scope: LiveSummaryScope::meeting(value.meeting_id),
                    utterance_id: value.utterance_id,
                    source_revision: unsigned_integer(value.source_revision)?,
                    source_event_id: value.source_event_id,
                    source_text_hash: value.source_text_hash,
                })
            })
            .collect::<Result<Vec<_>, LiveSummaryStoreError>>()?;
        items.push(LiveSummaryItem {
            item_id: item_row.item_id,
            kind: SummaryItemKind::parse(&item_row.kind)?,
            title: item_row.title,
            body: item_row.body,
            owner: item_row.owner,
            due_at: item_row.due_at,
            status: SummaryItemStatus::parse(&item_row.status)?,
            evidence,
        });
    }

    let revision = LiveSummaryRevision {
        summary_revision_id: row.summary_revision_id,
        scope: LiveSummaryScope::meeting(row.meeting_id),
        revision: unsigned_integer(row.revision)?,
        generation: unsigned_integer(row.generation)?,
        transcript_cursor: unsigned_integer(row.transcript_cursor)?,
        snapshot_hash: row.snapshot_hash,
        revision_type: LiveSummaryRevisionType::parse(&row.revision_type)?,
        provider: row.provider,
        model: row.model,
        created_at: row.created_at,
        items,
    };
    revision.validate()?;
    Ok(revision)
}

fn sqlite_integer(value: u64) -> Result<i64, LiveSummaryStoreError> {
    i64::try_from(value).map_err(|_| LiveSummaryStoreError::IntegerOutOfRange)
}

fn unsigned_integer(value: i64) -> Result<u64, LiveSummaryStoreError> {
    u64::try_from(value).map_err(|_| LiveSummaryStoreError::InvalidStoredState("negative integer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

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
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind("meeting-summary")
            .bind("Synthetic summary fixture")
            .bind("2026-09-02T00:00:00.000Z")
            .bind("2026-09-02T00:00:00.000Z")
            .execute(&pool)
            .await
            .expect("insert meeting");
        insert_transcript(&pool, "event-1", "utterance-1", 1, "确定范围").await;
        pool
    }

    async fn insert_transcript(
        pool: &SqlitePool,
        event_id: &str,
        utterance_id: &str,
        revision: i64,
        text: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO utterance_revisions (
                event_id,
                meeting_id,
                schema_version,
                utterance_id,
                revision,
                event_kind,
                is_stable,
                transcript,
                timestamp,
                created_at
            ) VALUES (?, 'meeting-summary', 1, ?, ?, 'final', 1, ?, '00:00:01', ?)
            "#,
        )
        .bind(event_id)
        .bind(utterance_id)
        .bind(revision)
        .bind(text)
        .bind("2026-09-02T00:00:01.000Z")
        .execute(pool)
        .await
        .expect("insert transcript revision");
    }

    async fn insert_ranked_transcript_event(
        pool: &SqlitePool,
        event_id: &str,
        utterance_id: &str,
        revision: i64,
        event_kind: &str,
        is_stable: bool,
        text: &str,
        created_at: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO utterance_revisions (
                event_id,
                meeting_id,
                schema_version,
                utterance_id,
                revision,
                event_kind,
                is_stable,
                transcript,
                timestamp,
                created_at
            ) VALUES (?, 'meeting-summary', 1, ?, ?, ?, ?, ?, '00:00:02', ?)
            "#,
        )
        .bind(event_id)
        .bind(utterance_id)
        .bind(revision)
        .bind(event_kind)
        .bind(is_stable)
        .bind(text)
        .bind(created_at)
        .execute(pool)
        .await
        .expect("insert ranked transcript event");
    }

    fn revision(
        revision_number: u64,
        generation: u64,
        summary_revision_id: &str,
    ) -> LiveSummaryRevision {
        let evidence = SummaryEvidence {
            scope: LiveSummaryScope::meeting("meeting-summary"),
            utterance_id: "utterance-1".to_string(),
            source_revision: 1,
            source_event_id: "event-1".to_string(),
            source_text_hash: source_text_hash("确定范围"),
        };
        LiveSummaryRevision {
            summary_revision_id: summary_revision_id.to_string(),
            scope: LiveSummaryScope::meeting("meeting-summary"),
            revision: revision_number,
            generation,
            transcript_cursor: 40 + revision_number,
            snapshot_hash: source_text_hash(&format!("snapshot-{generation}")),
            revision_type: LiveSummaryRevisionType::Live,
            provider: "deterministic-fake".to_string(),
            model: Some("fixture-v1".to_string()),
            created_at: format!("2026-09-02T00:00:{revision_number:02}.000Z"),
            items: vec![LiveSummaryItem {
                item_id: format!("topic-{revision_number}"),
                kind: SummaryItemKind::Topic,
                title: "范围".to_string(),
                body: "已经确定范围".to_string(),
                owner: None,
                due_at: None,
                status: SummaryItemStatus::Active,
                evidence: vec![evidence],
            }],
        }
    }

    #[tokio::test]
    async fn preferences_keep_custom_prompt_write_only_and_require_explicit_delete() {
        const PRIVATE_PROMPT: &str = "只突出发布风险，不要遗漏负责人";
        let pool = test_pool().await;
        let saved =
            LiveSummaryRepository::save_preferences(&pool, "daily_standup", Some(PRIVATE_PROMPT))
                .await
                .expect("save preferences");
        assert!(saved.has_custom_prompt);
        let public_json = serde_json::to_string(&saved).expect("serialize safe view");
        assert!(!public_json.contains(PRIVATE_PROMPT));
        assert!(!public_json.contains("customPrompt"));

        LiveSummaryRepository::save_preferences(&pool, "standard_meeting", None)
            .await
            .expect("change template without reading or replacing prompt");
        let runtime = LiveSummaryRepository::load_runtime_preferences(&pool)
            .await
            .expect("load private runtime settings");
        assert_eq!(runtime.template_id, "standard_meeting");
        assert_eq!(
            runtime
                .custom_prompt
                .as_ref()
                .map(|value| value.expose_secret()),
            Some(PRIVATE_PROMPT)
        );

        let deleted = LiveSummaryRepository::delete_custom_prompt(&pool)
            .await
            .expect("delete prompt explicitly");
        assert!(!deleted.has_custom_prompt);
        assert!(LiveSummaryRepository::load_runtime_preferences(&pool)
            .await
            .expect("load after delete")
            .custom_prompt
            .is_none());
    }

    #[tokio::test]
    async fn preferences_reject_empty_or_control_character_prompts() {
        let pool = test_pool().await;
        for prompt in ["   ", "unsafe\0prompt"] {
            assert!(LiveSummaryRepository::save_preferences(
                &pool,
                "standard_meeting",
                Some(prompt)
            )
            .await
            .is_err());
        }
    }

    #[tokio::test]
    async fn complete_revision_round_trips_with_items_evidence_and_cursor() {
        let pool = test_pool().await;
        let expected = revision(1, 1, "summary-revision-1");
        LiveSummaryRepository::persist_revision(&pool, &expected)
            .await
            .expect("persist revision");

        let loaded = LiveSummaryRepository::load_latest_revision(&pool, "meeting-summary")
            .await
            .expect("load latest")
            .expect("latest exists");
        assert_eq!(loaded, expected);
    }

    #[tokio::test]
    async fn delayed_generation_is_history_only_and_cannot_regress_latest_projection() {
        let pool = test_pool().await;
        let newer = revision(2, 5, "summary-newer-generation");
        LiveSummaryRepository::persist_revision(&pool, &newer)
            .await
            .expect("persist newer generation");
        let older_generation = revision(3, 4, "summary-delayed-generation");
        LiveSummaryRepository::persist_revision(&pool, &older_generation)
            .await
            .expect("keep delayed generation in history");

        let latest = LiveSummaryRepository::load_latest_revision(&pool, "meeting-summary")
            .await
            .expect("load latest")
            .expect("latest exists");
        assert_eq!(latest.summary_revision_id, "summary-newer-generation");
        assert_eq!(latest.generation, 5);
        let history_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM live_summary_revisions WHERE meeting_id = ?")
                .bind("meeting-summary")
                .fetch_one(&pool)
                .await
                .expect("count history");
        assert_eq!(history_count, 2);
    }

    #[tokio::test]
    async fn evidence_failure_rolls_back_revision_items_and_latest_projection() {
        let pool = test_pool().await;
        let mut invalid = revision(1, 1, "summary-must-roll-back");
        invalid.items[0].evidence[0].source_text_hash = source_text_hash("wrong text");
        let error = LiveSummaryRepository::persist_revision(&pool, &invalid)
            .await
            .expect_err("evidence mismatch must fail");
        assert!(matches!(error, LiveSummaryStoreError::EvidenceHashMismatch));

        let revision_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM live_summary_revisions WHERE summary_revision_id = ?",
        )
        .bind("summary-must-roll-back")
        .fetch_one(&pool)
        .await
        .expect("count revisions");
        let item_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM live_summary_items WHERE summary_revision_id = ?",
        )
        .bind("summary-must-roll-back")
        .fetch_one(&pool)
        .await
        .expect("count items");
        let session_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM live_summary_sessions WHERE meeting_id = ?")
                .bind("meeting-summary")
                .fetch_one(&pool)
                .await
                .expect("count sessions");
        assert_eq!((revision_count, item_count, session_count), (0, 0, 0));
    }

    #[tokio::test]
    async fn migration_has_all_tables_projection_trigger_and_cascading_edges() {
        let pool = test_pool().await;
        let tables: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM sqlite_master
            WHERE type = 'table'
              AND name IN (
                  'live_summary_sessions',
                  'live_summary_revisions',
                  'live_summary_items',
                  'live_summary_evidence'
              )
            "#,
        )
        .fetch_one(&pool)
        .await
        .expect("count tables");
        assert_eq!(tables, 4);

        let projection_trigger: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = ?",
        )
        .bind("trg_live_summary_revisions_project_latest")
        .fetch_one(&pool)
        .await
        .expect("find trigger");
        assert_eq!(projection_trigger, 1);

        let evidence_foreign_keys: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_foreign_key_list('live_summary_evidence')",
        )
        .fetch_one(&pool)
        .await
        .expect("count evidence foreign-key columns");
        assert!(evidence_foreign_keys >= 6);

        let persisted = revision(1, 1, "summary-cascade");
        LiveSummaryRepository::persist_revision(&pool, &persisted)
            .await
            .expect("persist cascade fixture");
        sqlx::query("DELETE FROM meetings WHERE id = ?")
            .bind("meeting-summary")
            .execute(&pool)
            .await
            .expect("delete meeting");
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM live_summary_revisions WHERE meeting_id = ?")
                .bind("meeting-summary")
                .fetch_one(&pool)
                .await
                .expect("count cascaded revisions");
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn recovery_selects_one_canonical_head_for_same_revision_events() {
        let pool = test_pool().await;
        insert_ranked_transcript_event(
            &pool,
            "priority-partial",
            "utterance-priority",
            7,
            "partial",
            false,
            "草稿",
            "2026-09-02T00:00:05.000Z",
        )
        .await;
        insert_ranked_transcript_event(
            &pool,
            "priority-final",
            "utterance-priority",
            7,
            "final",
            true,
            "最终文本",
            "2026-09-02T00:00:04.000Z",
        )
        .await;
        insert_ranked_transcript_event(
            &pool,
            "priority-retraction",
            "utterance-priority",
            7,
            "retraction",
            true,
            "",
            "2026-09-02T00:00:03.000Z",
        )
        .await;

        let recovery = LiveSummaryRepository::load_recovery_snapshot(&pool, "meeting-summary")
            .await
            .expect("load recovery snapshot");
        let priority_heads = recovery
            .transcript_heads
            .iter()
            .filter(|event| event.utterance_id == "utterance-priority")
            .collect::<Vec<_>>();

        assert_eq!(recovery.current_transcript_cursor, 3);
        assert_eq!(priority_heads.len(), 1);
        assert_eq!(priority_heads[0].event_id, "priority-retraction");
        assert_eq!(
            priority_heads[0].event_kind,
            TranscriptEventKind::Retraction
        );
    }
}
