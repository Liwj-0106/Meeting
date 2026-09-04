use crate::audio::transcription::translation::{
    source_text_hash, TranslationErrorCode, TranslationEvent, TranslationEventKind,
    TranslationRequestFingerprint, TranslationSourceEvent, TranslationSourceKind,
    TranslationStatus, TRANSLATION_EVENT_SCHEMA_VERSION,
};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use std::collections::{HashMap, HashSet};
use std::fmt;
use thiserror::Error;

const ORPHANED_STAGED_TRANSLATION_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Clone, PartialEq, Eq)]
pub struct LiveTranslationSecret(String);

impl LiveTranslationSecret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LiveTranslationSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LiveTranslationSecret(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTranslationSettingsRecord {
    pub enabled: bool,
    pub source_language: String,
    pub target_language: String,
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub api_key: Option<LiveTranslationSecret>,
}

#[derive(Debug, FromRow)]
struct LiveTranslationSettingsRow {
    enabled: bool,
    source_language: String,
    target_language: String,
    provider: String,
    model: String,
    endpoint: String,
    api_key: Option<String>,
}

impl From<LiveTranslationSettingsRow> for LiveTranslationSettingsRecord {
    fn from(row: LiveTranslationSettingsRow) -> Self {
        Self {
            enabled: row.enabled,
            source_language: row.source_language,
            target_language: row.target_language,
            provider: row.provider,
            model: row.model,
            endpoint: row.endpoint,
            api_key: row.api_key.map(LiveTranslationSecret::new),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationPersistenceOutcome {
    Inserted,
    AlreadyPresent,
    SourceNotPersisted,
    SourceSuperseded,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranslationSessionBindReport {
    pub inserted: usize,
    pub already_present: usize,
    pub rejected_missing_source: usize,
    pub rejected_superseded_source: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranslationStagingPromotionReport {
    pub attempted_sessions: usize,
    pub processed_sessions: usize,
    pub promoted_sessions: usize,
    pub inserted: usize,
    pub already_present: usize,
    pub rejected_missing_source: usize,
    pub rejected_superseded_source: usize,
}

impl TranslationSessionBindReport {
    pub fn persisted(&self) -> usize {
        self.inserted + self.already_present
    }

    pub fn rejected(&self) -> usize {
        self.rejected_missing_source + self.rejected_superseded_source
    }
}

#[derive(Debug, Error)]
pub enum TranslationRepositoryError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("persisted transcript text does not match the translation source hash")]
    SourceHashMismatch,
    #[error("translation source scope does not match the trusted recording session")]
    SourceScopeMismatch,
    #[error("translation event ID conflicts with an existing immutable event")]
    EventIdConflict,
    #[error("stored staged translation event is invalid")]
    InvalidStagedEvent,
    #[error("translation binding identifiers are invalid")]
    InvalidBinding,
    #[error("translation counter exceeds SQLite integer range: {0}")]
    CounterOverflow(&'static str),
    #[error(
        "staged translation promotion failed for {failed_sessions} of {attempted_sessions} sessions after processing {processed_sessions} sessions and promoting {promoted_sessions}"
    )]
    StagedPromotionPartial {
        attempted_sessions: usize,
        processed_sessions: usize,
        promoted_sessions: usize,
        failed_sessions: usize,
    },
}

#[derive(Debug, FromRow)]
struct StagedTranslationRow {
    translation_event_id: String,
    session_id: String,
    utterance_id: String,
    source_event_id: String,
    source_revision: i64,
    source_kind: String,
    source_text_hash: String,
    target_language: String,
    generation: i64,
    translation_revision: i64,
    event_json: String,
}

pub struct TranslationRepository;

impl TranslationRepository {
    pub async fn get_settings(
        pool: &SqlitePool,
    ) -> Result<LiveTranslationSettingsRecord, sqlx::Error> {
        let row = sqlx::query_as::<_, LiveTranslationSettingsRow>(
            r#"
            SELECT enabled, source_language, target_language, provider, model, endpoint, api_key
            FROM live_translation_settings
            WHERE id = 1
            "#,
        )
        .fetch_one(pool)
        .await?;
        Ok(row.into())
    }

    /// Saves the public configuration while deliberately preserving the
    /// Rust-only credential column.
    pub async fn save_settings(
        pool: &SqlitePool,
        settings: &LiveTranslationSettingsRecord,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE live_translation_settings
            SET enabled = ?, source_language = ?, target_language = ?,
                provider = ?, model = ?, endpoint = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = 1
            "#,
        )
        .bind(settings.enabled)
        .bind(&settings.source_language)
        .bind(&settings.target_language)
        .bind(&settings.provider)
        .bind(&settings.model)
        .bind(&settings.endpoint)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn set_api_key(pool: &SqlitePool, api_key: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE live_translation_settings SET api_key = ?, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
        )
        .bind(api_key)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn clear_api_key(pool: &SqlitePool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE live_translation_settings SET api_key = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn load_high_watermark(
        pool: &SqlitePool,
        meeting_id: &str,
        utterance_id: &str,
        target_language: &str,
    ) -> Result<Option<(u64, u64)>, TranslationRepositoryError> {
        let counters = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
            r#"
            SELECT MAX(generation), MAX(translation_revision)
            FROM (
                SELECT generation, translation_revision
                FROM translation_revisions
                WHERE meeting_id = ? AND utterance_id = ? AND target_language = ?
                UNION ALL
                SELECT generation, translation_revision
                FROM live_translation_staging
                WHERE session_id = ? AND utterance_id = ? AND target_language = ?
            )
            "#,
        )
        .bind(meeting_id)
        .bind(utterance_id)
        .bind(target_language)
        .bind(meeting_id)
        .bind(utterance_id)
        .bind(target_language)
        .fetch_one(pool)
        .await?;
        match counters {
            (Some(generation), Some(revision)) => Ok(Some((
                u64::try_from(generation)
                    .map_err(|_| TranslationRepositoryError::CounterOverflow("generation"))?,
                u64::try_from(revision).map_err(|_| {
                    TranslationRepositoryError::CounterOverflow("translation_revision")
                })?,
            ))),
            _ => Ok(None),
        }
    }

    /// Durably stages a complete Rust-owned translation while its recording
    /// scope is still an ASR session rather than a canonical meeting. The
    /// serialized event is immutable and all lookup columns are checked again
    /// when it is read back.
    pub async fn stage_session_event(
        pool: &SqlitePool,
        event: &TranslationEvent,
    ) -> Result<TranslationPersistenceOutcome, TranslationRepositoryError> {
        validate_staged_event_shape(event)?;
        let event_json = serde_json::to_string(event)
            .map_err(|_| TranslationRepositoryError::InvalidStagedEvent)?;
        let staged_at_ms = chrono::Utc::now().timestamp_millis().max(0);
        let mut transaction = pool.begin().await?;
        prune_expired_orphaned_sessions(&mut transaction, staged_at_ms, &event.source.meeting_id)
            .await?;

        if let Some(existing_json) = sqlx::query_scalar::<_, String>(
            "SELECT event_json FROM live_translation_staging WHERE translation_event_id = ?",
        )
        .bind(&event.translation_event_id)
        .fetch_optional(&mut *transaction)
        .await?
        {
            let existing = deserialize_staged_event(&existing_json)?;
            if existing == *event {
                transaction.commit().await?;
                return Ok(TranslationPersistenceOutcome::AlreadyPresent);
            }
            return Err(TranslationRepositoryError::EventIdConflict);
        }

        sqlx::query(
            r#"
            INSERT INTO live_translation_staging (
                translation_event_id, session_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                target_language, generation, translation_revision,
                event_json, staged_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&event.translation_event_id)
        .bind(&event.source.meeting_id)
        .bind(&event.source.utterance_id)
        .bind(&event.source.event_id)
        .bind(sqlite_counter(event.source.revision, "source_revision")?)
        .bind(source_kind_name(event.source_kind))
        .bind(&event.source.text_hash)
        .bind(&event.target_language)
        .bind(sqlite_counter(event.generation, "generation")?)
        .bind(sqlite_counter(
            event.translation_revision,
            "translation_revision",
        )?)
        .bind(event_json)
        .bind(staged_at_ms)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(TranslationPersistenceOutcome::Inserted)
    }

    /// Returns the newest durable snapshot only when every source identity and
    /// translation input still matches the current caption request. A stale
    /// source revision or an old provider/model configuration therefore cannot
    /// be replayed into the overlay.
    pub async fn load_staged_event_for_source(
        pool: &SqlitePool,
        source: &TranslationSourceEvent,
        fingerprint: &TranslationRequestFingerprint,
    ) -> Result<Option<TranslationEvent>, TranslationRepositoryError> {
        let rows = sqlx::query_as::<_, StagedTranslationRow>(
            r#"
            SELECT
                translation_event_id, session_id, utterance_id,
                source_event_id, source_revision, source_kind, source_text_hash,
                target_language, generation, translation_revision, event_json
            FROM live_translation_staging
            WHERE session_id = ? AND utterance_id = ?
              AND source_event_id = ? AND source_revision = ?
              AND source_kind = ? AND source_text_hash = ?
              AND target_language = ?
            ORDER BY generation DESC, translation_revision DESC, translation_event_id DESC
            "#,
        )
        .bind(&source.source.meeting_id)
        .bind(&source.source.utterance_id)
        .bind(&source.source.event_id)
        .bind(sqlite_counter(source.source.revision, "source_revision")?)
        .bind(source_kind_name(source.event_kind))
        .bind(&source.source.text_hash)
        .bind(&fingerprint.target_language)
        .fetch_all(pool)
        .await?;

        let mut restored = None;
        for row in rows {
            let event = event_from_staged_row(row)?;
            if event.request_fingerprint == *fingerprint
                && event.source_language == fingerprint.source_language
                && event.target_language == fingerprint.target_language
            {
                restored = Some(event);
                break;
            }
        }
        Ok(restored)
    }

    /// Persists a complete translation event only when its canonical source
    /// has already reached `utterance_revisions` and remains the current
    /// immutable source version for its utterance.
    pub async fn insert_event_if_source_exists(
        pool: &SqlitePool,
        event: &TranslationEvent,
    ) -> Result<TranslationPersistenceOutcome, TranslationRepositoryError> {
        let mut transaction = pool.begin().await?;
        let outcome = insert_validated_event(&mut transaction, event, None, true).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    /// Rebinds Rust-owned active-session events to the canonical meeting that
    /// was just committed. Renderer text never enters this API. Every event is
    /// checked against the exact saved source tuple and SHA-256 hash inside the
    /// same transaction. Superseded translations are skipped, except for a
    /// completed snapshot required by a current `reused` event.
    pub async fn bind_session_events(
        pool: &SqlitePool,
        source_session_id: &str,
        meeting_id: &str,
        events: &[TranslationEvent],
    ) -> Result<TranslationSessionBindReport, TranslationRepositoryError> {
        if !valid_binding_id(source_session_id) || !valid_binding_id(meeting_id) {
            return Err(TranslationRepositoryError::InvalidBinding);
        }

        let mut transaction = pool.begin().await?;
        let staged_events = load_staged_session_events(&mut transaction, source_session_id).await?;
        let staged_ids = staged_events
            .iter()
            .map(|event| event.translation_event_id.clone())
            .collect::<HashSet<_>>();
        let mut merged_events = HashMap::<String, TranslationEvent>::new();
        for event in staged_events.iter().chain(events.iter()) {
            match merged_events.get(&event.translation_event_id) {
                Some(existing) if existing != event => {
                    return Err(TranslationRepositoryError::EventIdConflict)
                }
                Some(_) => {}
                None => {
                    merged_events.insert(event.translation_event_id.clone(), event.clone());
                }
            }
        }
        let mut merged_events = merged_events.into_values().collect::<Vec<_>>();
        merged_events.sort_by(|left, right| {
            left.translation_revision
                .cmp(&right.translation_revision)
                .then(left.generation.cmp(&right.generation))
                .then(left.created_at_ms.cmp(&right.created_at_ms))
                .then(left.translation_event_id.cmp(&right.translation_event_id))
        });

        let mut report = TranslationSessionBindReport::default();
        let mut candidates = Vec::new();
        let mut consumed_staged_ids = HashSet::new();

        for event in &merged_events {
            if event.source.meeting_id != source_session_id {
                return Err(TranslationRepositoryError::SourceScopeMismatch);
            }
            let mut rebound = event.clone();
            rebound.source.meeting_id = meeting_id.to_string();

            match existing_event_matches(&mut transaction, &rebound).await? {
                Some(true) => {
                    report.already_present += 1;
                    if staged_ids.contains(&event.translation_event_id) {
                        consumed_staged_ids.insert(event.translation_event_id.clone());
                    }
                    continue;
                }
                Some(false) => return Err(TranslationRepositoryError::EventIdConflict),
                None => {}
            }

            match validate_source(&mut transaction, &rebound, Some(source_session_id)).await? {
                SourceValidation::Missing => report.rejected_missing_source += 1,
                SourceValidation::Current => {
                    if staged_ids.contains(&event.translation_event_id) {
                        consumed_staged_ids.insert(event.translation_event_id.clone());
                    }
                    candidates.push((rebound, true));
                }
                SourceValidation::Superseded => {
                    if staged_ids.contains(&event.translation_event_id) {
                        consumed_staged_ids.insert(event.translation_event_id.clone());
                    }
                    candidates.push((rebound, false));
                }
            }
        }

        let by_id = candidates
            .iter()
            .map(|(event, _)| {
                (
                    event.translation_event_id.clone(),
                    event.reused_from_event_id.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut dependencies = candidates
            .iter()
            .filter(|(_, current)| *current)
            .filter_map(|(event, _)| event.reused_from_event_id.clone())
            .collect::<HashSet<_>>();
        loop {
            let before = dependencies.len();
            let parents = dependencies.iter().cloned().collect::<Vec<_>>();
            for parent in parents {
                if let Some(reused_from_event_id) = by_id.get(&parent) {
                    if let Some(ancestor) = reused_from_event_id.as_ref() {
                        dependencies.insert(ancestor.clone());
                    }
                }
            }
            if dependencies.len() == before {
                break;
            }
        }

        let mut eligible = Vec::new();
        for (event, current) in candidates {
            if current || dependencies.contains(&event.translation_event_id) {
                eligible.push(event);
            } else {
                report.rejected_superseded_source += 1;
            }
        }
        eligible.sort_by(|left, right| {
            left.translation_revision
                .cmp(&right.translation_revision)
                .then(left.generation.cmp(&right.generation))
                .then(left.created_at_ms.cmp(&right.created_at_ms))
                .then(left.translation_event_id.cmp(&right.translation_event_id))
        });

        for event in eligible {
            insert_translation_row(&mut transaction, &event).await?;
            report.inserted += 1;
        }
        for event_id in consumed_staged_ids {
            sqlx::query(
                "DELETE FROM live_translation_staging WHERE session_id = ? AND translation_event_id = ?",
            )
            .bind(source_session_id)
            .bind(event_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(report)
    }

    /// Repairs the intentional transaction boundary between saving a meeting
    /// and promoting staged translations. The durable binding table is the
    /// only source of session-to-meeting identity; callers never supply a
    /// renderer-controlled scope. Repeated calls are safe because a successful
    /// promotion deletes its staged rows in the same transaction as inserts.
    pub async fn promote_bound_staged_sessions(
        pool: &SqlitePool,
    ) -> Result<TranslationStagingPromotionReport, TranslationRepositoryError> {
        let bindings = sqlx::query_as::<_, (String, String)>(
            r#"
            SELECT DISTINCT binding.source_session_id, binding.meeting_id
            FROM recording_session_meeting_bindings AS binding
            INNER JOIN live_translation_staging AS staging
                ON staging.session_id = binding.source_session_id
            WHERE binding.state = 'bound'
              AND binding.meeting_id IS NOT NULL
            ORDER BY binding.source_session_id
            "#,
        )
        .fetch_all(pool)
        .await?;

        let mut report = TranslationStagingPromotionReport {
            attempted_sessions: bindings.len(),
            ..TranslationStagingPromotionReport::default()
        };
        let mut failed_sessions = 0;
        for (source_session_id, meeting_id) in bindings {
            match Self::bind_session_events(pool, &source_session_id, &meeting_id, &[]).await {
                Ok(binding) => {
                    report.processed_sessions += 1;
                    if binding.persisted() > 0 {
                        report.promoted_sessions += 1;
                    }
                    report.inserted += binding.inserted;
                    report.already_present += binding.already_present;
                    report.rejected_missing_source += binding.rejected_missing_source;
                    report.rejected_superseded_source += binding.rejected_superseded_source;
                }
                Err(_) => failed_sessions += 1,
            }
        }
        if failed_sessions > 0 {
            return Err(TranslationRepositoryError::StagedPromotionPartial {
                attempted_sessions: report.attempted_sessions,
                processed_sessions: report.processed_sessions,
                promoted_sessions: report.promoted_sessions,
                failed_sessions,
            });
        }
        Ok(report)
    }
}

async fn prune_expired_orphaned_sessions(
    transaction: &mut Transaction<'_, Sqlite>,
    now_ms: i64,
    active_session_id: &str,
) -> Result<(), TranslationRepositoryError> {
    let cutoff_ms = now_ms.saturating_sub(ORPHANED_STAGED_TRANSLATION_RETENTION_MS);
    // Prune only whole unbound/pending sessions when both the newest staged
    // event and the durable binding heartbeat are stale. The session currently
    // writing remains protected even during an unusually long meeting.
    sqlx::query(
        r#"
        DELETE FROM live_translation_staging
        WHERE session_id IN (
            SELECT staging.session_id
            FROM live_translation_staging AS staging
            LEFT JOIN recording_session_meeting_bindings AS binding
                ON binding.source_session_id = staging.session_id
            WHERE (binding.source_session_id IS NULL OR binding.state = 'pending')
              AND staging.session_id <> ?
            GROUP BY staging.session_id
              HAVING MAX(staging.staged_at_ms) < ?
                 AND COALESCE(
                     MAX(CAST(strftime('%s', binding.updated_at) AS INTEGER) * 1000),
                     0
                 ) < ?
        )
        "#,
    )
    .bind(active_session_id)
    .bind(cutoff_ms)
    .bind(cutoff_ms)
    .execute(&mut **transaction)
    .await?;

    // Once the last staged row is gone, remove the matching abandoned pending
    // anchor as well. Bound rows are never deleted here and remain available
    // for the promotion compensator.
    sqlx::query(
        r#"
        DELETE FROM recording_session_meeting_bindings
        WHERE state = 'pending'
          AND source_session_id <> ?
          AND COALESCE(
              CAST(strftime('%s', updated_at) AS INTEGER) * 1000,
              0
          ) < ?
          AND NOT EXISTS (
              SELECT 1
              FROM live_translation_staging AS staging
              WHERE staging.session_id = recording_session_meeting_bindings.source_session_id
          )
        "#,
    )
    .bind(active_session_id)
    .bind(cutoff_ms)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn load_staged_session_events(
    transaction: &mut Transaction<'_, Sqlite>,
    session_id: &str,
) -> Result<Vec<TranslationEvent>, TranslationRepositoryError> {
    let rows = sqlx::query_as::<_, StagedTranslationRow>(
        r#"
        SELECT
            translation_event_id, session_id, utterance_id,
            source_event_id, source_revision, source_kind, source_text_hash,
            target_language, generation, translation_revision, event_json
        FROM live_translation_staging
        WHERE session_id = ?
        ORDER BY translation_revision, generation, translation_event_id
        "#,
    )
    .bind(session_id)
    .fetch_all(&mut **transaction)
    .await?;
    rows.into_iter().map(event_from_staged_row).collect()
}

fn event_from_staged_row(
    row: StagedTranslationRow,
) -> Result<TranslationEvent, TranslationRepositoryError> {
    let event = deserialize_staged_event(&row.event_json)?;
    validate_staged_event_shape(&event)?;
    let matches = row.translation_event_id == event.translation_event_id
        && row.session_id == event.source.meeting_id
        && row.utterance_id == event.source.utterance_id
        && row.source_event_id == event.source.event_id
        && row.source_revision == sqlite_counter(event.source.revision, "source_revision")?
        && row.source_kind == source_kind_name(event.source_kind)
        && row.source_text_hash == event.source.text_hash
        && row.target_language == event.target_language
        && row.generation == sqlite_counter(event.generation, "generation")?
        && row.translation_revision
            == sqlite_counter(event.translation_revision, "translation_revision")?;
    if !matches {
        return Err(TranslationRepositoryError::InvalidStagedEvent);
    }
    Ok(event)
}

fn deserialize_staged_event(value: &str) -> Result<TranslationEvent, TranslationRepositoryError> {
    serde_json::from_str(value).map_err(|_| TranslationRepositoryError::InvalidStagedEvent)
}

fn validate_staged_event_shape(event: &TranslationEvent) -> Result<(), TranslationRepositoryError> {
    if event.schema_version != TRANSLATION_EVENT_SCHEMA_VERSION
        || !valid_binding_id(&event.translation_event_id)
        || !valid_binding_id(&event.source.meeting_id)
        || !valid_binding_id(&event.source.event_id)
        || !valid_binding_id(&event.source.utterance_id)
        || event.translation_revision == 0
        || event.generation == 0
        || !valid_language_tag(&event.source_language)
        || !valid_language_tag(&event.target_language)
        || event
            .source_language
            .eq_ignore_ascii_case(&event.target_language)
        || event.source.text_hash.len() != 64
        || !event
            .source
            .text_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || event.request_fingerprint.source_language != event.source_language
        || event.request_fingerprint.target_language != event.target_language
        || event.request_fingerprint.provider.trim().is_empty()
        || event.request_fingerprint.provider.len() > 128
        || (event.status != TranslationStatus::Retracted
            && event.request_fingerprint.model != event.model)
        || event.request_fingerprint.glossary != event.glossary
    {
        return Err(TranslationRepositoryError::InvalidStagedEvent);
    }
    if event
        .provider
        .as_ref()
        .is_some_and(|provider| provider != &event.request_fingerprint.provider)
    {
        return Err(TranslationRepositoryError::InvalidStagedEvent);
    }
    let valid_status = match event.status {
        TranslationStatus::Partial => {
            event.event_kind == TranslationEventKind::Snapshot
                && event.source_kind == TranslationSourceKind::Partial
                && valid_translation_snapshot(event)
                && event.reused_from_event_id.is_none()
                && event.error.is_none()
        }
        TranslationStatus::Final => {
            event.event_kind == TranslationEventKind::Snapshot
                && matches!(
                    event.source_kind,
                    TranslationSourceKind::Final
                        | TranslationSourceKind::Correction
                        | TranslationSourceKind::SpeakerUpdate
                        | TranslationSourceKind::LanguageUpdate
                )
                && valid_translation_snapshot(event)
                && event.reused_from_event_id.is_none()
                && event.error.is_none()
        }
        TranslationStatus::Reused => {
            event.event_kind == TranslationEventKind::Snapshot
                && event.source_kind == TranslationSourceKind::SpeakerUpdate
                && valid_translation_snapshot(event)
                && event
                    .reused_from_event_id
                    .as_deref()
                    .is_some_and(valid_binding_id)
                && event.error.is_none()
        }
        TranslationStatus::Retracted => {
            event.event_kind == TranslationEventKind::Retraction
                && event.source_kind == TranslationSourceKind::Retraction
                && event.translated_text.is_none()
                && event.provider.is_none()
                && event.model.is_none()
                && event.reused_from_event_id.is_none()
                && event.latency_ms.is_none()
                && event.error.is_none()
        }
        TranslationStatus::Failed => {
            event.event_kind == TranslationEventKind::Error
                && event.source_kind != TranslationSourceKind::Retraction
                && event.translated_text.is_none()
                && event.provider.as_deref() == Some(event.request_fingerprint.provider.as_str())
                && event.reused_from_event_id.is_none()
                && event.latency_ms.is_none()
                && event.error.is_some()
        }
    };
    if !valid_status {
        return Err(TranslationRepositoryError::InvalidStagedEvent);
    }
    sqlite_counter(event.source.revision, "source_revision")?;
    sqlite_counter(event.generation, "generation")?;
    sqlite_counter(event.translation_revision, "translation_revision")?;
    Ok(())
}

fn valid_translation_snapshot(event: &TranslationEvent) -> bool {
    event.translated_text.as_deref().is_some_and(|text| {
        !text.trim().is_empty() && text.chars().count() <= 65_536 && !text.contains('\0')
    }) && event.provider.as_deref() == Some(event.request_fingerprint.provider.as_str())
}

fn valid_language_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 35
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceValidation {
    Missing,
    Current,
    Superseded,
}

#[derive(Debug, FromRow)]
struct ExistingTranslationIdentity {
    meeting_id: String,
    utterance_id: String,
    source_event_id: String,
    source_revision: i64,
    source_kind: String,
    source_text_hash: String,
    translation_revision: i64,
    generation: i64,
    event_kind: String,
    status: String,
    source_language: String,
    target_language: String,
    translated_text: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    speaker_id: Option<String>,
    reused_from_event_id: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    retryable: Option<bool>,
}

async fn existing_event_matches(
    transaction: &mut Transaction<'_, Sqlite>,
    event: &TranslationEvent,
) -> Result<Option<bool>, TranslationRepositoryError> {
    let existing = sqlx::query_as::<_, ExistingTranslationIdentity>(
        r#"
        SELECT
            meeting_id, utterance_id, source_event_id, source_revision,
            source_kind, source_text_hash, translation_revision, generation,
            event_kind, status, source_language, target_language,
            translated_text, provider, model, speaker_id,
            reused_from_event_id, error_code, error_message, retryable
        FROM translation_revisions
        WHERE translation_event_id = ?
        "#,
    )
    .bind(&event.translation_event_id)
    .fetch_optional(&mut **transaction)
    .await?;

    let Some(existing) = existing else {
        return Ok(None);
    };
    let (error_code, error_message, retryable) = event
        .error
        .as_ref()
        .map(|error| {
            (
                Some(error_code_name(error.code).to_string()),
                Some(error.message.clone()),
                Some(error.retryable),
            )
        })
        .unwrap_or((None, None, None));
    let matches = existing.meeting_id == event.source.meeting_id
        && existing.utterance_id == event.source.utterance_id
        && existing.source_event_id == event.source.event_id
        && existing.source_revision == sqlite_counter(event.source.revision, "source_revision")?
        && existing.source_kind == source_kind_name(event.source_kind)
        && existing.source_text_hash == event.source.text_hash
        && existing.translation_revision
            == sqlite_counter(event.translation_revision, "translation_revision")?
        && existing.generation == sqlite_counter(event.generation, "generation")?
        && existing.event_kind == event_kind_name(event.event_kind)
        && existing.status == status_name(event.status)
        && existing.source_language == event.source_language
        && existing.target_language == event.target_language
        && existing.translated_text == event.translated_text
        && existing.provider == event.provider
        && existing.model == event.model
        && existing.speaker_id == event.speaker_id
        && existing.reused_from_event_id == event.reused_from_event_id
        && existing.error_code == error_code
        && existing.error_message == error_message
        && existing.retryable == retryable;
    Ok(Some(matches))
}

async fn validate_source(
    transaction: &mut Transaction<'_, Sqlite>,
    event: &TranslationEvent,
    expected_session_id: Option<&str>,
) -> Result<SourceValidation, TranslationRepositoryError> {
    let source_kind = source_kind_name(event.source_kind);
    let source_text = if let Some(session_id) = expected_session_id {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT transcript
            FROM utterance_revisions
            WHERE event_id = ? AND meeting_id = ? AND session_id = ?
              AND utterance_id = ? AND revision = ? AND event_kind = ?
            "#,
        )
        .bind(&event.source.event_id)
        .bind(&event.source.meeting_id)
        .bind(session_id)
        .bind(&event.source.utterance_id)
        .bind(sqlite_counter(event.source.revision, "source_revision")?)
        .bind(source_kind)
        .fetch_optional(&mut **transaction)
        .await?
    } else {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT transcript
            FROM utterance_revisions
            WHERE event_id = ? AND meeting_id = ? AND utterance_id = ?
              AND revision = ? AND event_kind = ?
            "#,
        )
        .bind(&event.source.event_id)
        .bind(&event.source.meeting_id)
        .bind(&event.source.utterance_id)
        .bind(sqlite_counter(event.source.revision, "source_revision")?)
        .bind(source_kind)
        .fetch_optional(&mut **transaction)
        .await?
    };

    let Some(source_text) = source_text else {
        return Ok(SourceValidation::Missing);
    };
    if source_text_hash(&source_text) != event.source.text_hash {
        return Err(TranslationRepositoryError::SourceHashMismatch);
    }

    let current_event_id = sqlx::query_scalar::<_, String>(
        r#"
        SELECT event_id
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
    .bind(&event.source.meeting_id)
    .bind(&event.source.utterance_id)
    .fetch_one(&mut **transaction)
    .await?;
    Ok(if current_event_id == event.source.event_id {
        SourceValidation::Current
    } else {
        SourceValidation::Superseded
    })
}

async fn insert_validated_event(
    transaction: &mut Transaction<'_, Sqlite>,
    event: &TranslationEvent,
    expected_session_id: Option<&str>,
    require_current_source: bool,
) -> Result<TranslationPersistenceOutcome, TranslationRepositoryError> {
    match existing_event_matches(transaction, event).await? {
        Some(true) => return Ok(TranslationPersistenceOutcome::AlreadyPresent),
        Some(false) => return Err(TranslationRepositoryError::EventIdConflict),
        None => {}
    }
    match validate_source(transaction, event, expected_session_id).await? {
        SourceValidation::Missing => return Ok(TranslationPersistenceOutcome::SourceNotPersisted),
        SourceValidation::Superseded if require_current_source => {
            return Ok(TranslationPersistenceOutcome::SourceSuperseded)
        }
        SourceValidation::Current | SourceValidation::Superseded => {}
    }
    insert_translation_row(transaction, event).await?;
    Ok(TranslationPersistenceOutcome::Inserted)
}

async fn insert_translation_row(
    transaction: &mut Transaction<'_, Sqlite>,
    event: &TranslationEvent,
) -> Result<(), TranslationRepositoryError> {
    let (glossary_id, glossary_version, glossary_content_hash) =
        if let Some(glossary) = event.glossary.as_ref() {
            (
                Some(glossary.glossary_id.as_str()),
                Some(sqlite_counter(glossary.version, "glossary_version")?),
                Some(glossary.content_hash.as_str()),
            )
        } else {
            (None, None, None)
        };
    let (error_code, error_message, retryable) = event
        .error
        .as_ref()
        .map(|error| {
            (
                Some(error_code_name(error.code)),
                Some(error.message.as_str()),
                Some(error.retryable),
            )
        })
        .unwrap_or((None, None, None));

    sqlx::query(
        r#"
        INSERT INTO translation_revisions (
            translation_event_id, meeting_id, utterance_id,
            source_event_id, source_revision, source_kind, source_text_hash,
            translation_revision, generation, event_kind, status,
            source_language, target_language, translated_text,
            provider, model, speaker_id,
            glossary_id, glossary_version, glossary_content_hash,
            reused_from_event_id, latency_ms,
            error_code, error_message, retryable, created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&event.translation_event_id)
    .bind(&event.source.meeting_id)
    .bind(&event.source.utterance_id)
    .bind(&event.source.event_id)
    .bind(sqlite_counter(event.source.revision, "source_revision")?)
    .bind(source_kind_name(event.source_kind))
    .bind(&event.source.text_hash)
    .bind(sqlite_counter(
        event.translation_revision,
        "translation_revision",
    )?)
    .bind(sqlite_counter(event.generation, "generation")?)
    .bind(event_kind_name(event.event_kind))
    .bind(status_name(event.status))
    .bind(&event.source_language)
    .bind(&event.target_language)
    .bind(event.translated_text.as_deref())
    .bind(event.provider.as_deref())
    .bind(event.model.as_deref())
    .bind(event.speaker_id.as_deref())
    .bind(glossary_id)
    .bind(glossary_version)
    .bind(glossary_content_hash)
    .bind(event.reused_from_event_id.as_deref())
    .bind(event.latency_ms.and_then(|value| i64::try_from(value).ok()))
    .bind(error_code)
    .bind(error_message)
    .bind(retryable)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn valid_binding_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn sqlite_counter(value: u64, field: &'static str) -> Result<i64, TranslationRepositoryError> {
    i64::try_from(value).map_err(|_| TranslationRepositoryError::CounterOverflow(field))
}

fn source_kind_name(kind: TranslationSourceKind) -> &'static str {
    match kind {
        TranslationSourceKind::Partial => "partial",
        TranslationSourceKind::Final => "final",
        TranslationSourceKind::Correction => "correction",
        TranslationSourceKind::SpeakerUpdate => "speaker_update",
        TranslationSourceKind::LanguageUpdate => "language_update",
        TranslationSourceKind::Retraction => "retraction",
    }
}

fn event_kind_name(kind: TranslationEventKind) -> &'static str {
    match kind {
        TranslationEventKind::Snapshot => "snapshot",
        TranslationEventKind::Retraction => "retraction",
        TranslationEventKind::Error => "error",
    }
}

fn status_name(status: TranslationStatus) -> &'static str {
    match status {
        TranslationStatus::Partial => "partial",
        TranslationStatus::Final => "final",
        TranslationStatus::Reused => "reused",
        TranslationStatus::Retracted => "retracted",
        TranslationStatus::Failed => "failed",
    }
}

fn error_code_name(code: TranslationErrorCode) -> &'static str {
    match code {
        TranslationErrorCode::ProviderRejected => "provider_rejected",
        TranslationErrorCode::ProviderFailed => "provider_failed",
        TranslationErrorCode::InvalidResponse => "invalid_response",
    }
}
