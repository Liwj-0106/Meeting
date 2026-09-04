use chrono::Utc;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use std::path::{Component, Path};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct RecordingSessionMeetingBinding {
    pub source_session_id: String,
    pub recording_folder_hash: String,
    pub meeting_id: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingSessionBindOutcome {
    PendingCompleted,
    AlreadyBound,
}

#[derive(Debug, Error)]
pub enum RecordingSessionBindingStoreError {
    #[error("recording session identifier is invalid")]
    InvalidSessionId,
    #[error("recording folder correlation is invalid")]
    InvalidFolderHash,
    #[error("recording session has no durable pending start anchor")]
    MissingPending,
    #[error("recording session is already correlated with another folder or meeting")]
    SessionConflict,
    #[error("recording folder is already correlated with another session")]
    FolderConflict,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct RecordingSessionBindingRepository;

impl RecordingSessionBindingRepository {
    /// Hash a normalized native path without first converting it through a
    /// lossy Unicode string. Existing folders are canonicalized so the start
    /// and recovery paths produce the same correlation value.
    pub fn recording_folder_hash(path: &Path) -> Result<String, RecordingSessionBindingStoreError> {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !resolved.is_absolute()
            || resolved
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(RecordingSessionBindingStoreError::InvalidFolderHash);
        }

        #[cfg(windows)]
        let native_bytes = {
            use std::os::windows::ffi::OsStrExt;

            let mut bytes = Vec::new();
            for unit in resolved.as_os_str().encode_wide() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes
        };

        #[cfg(unix)]
        let native_bytes = {
            use std::os::unix::ffi::OsStrExt;
            resolved.as_os_str().as_bytes().to_vec()
        };

        let digest = Sha256::digest(native_bytes);
        Ok(format!("{digest:x}"))
    }

    /// Register the Rust-created session before transcript work starts. This
    /// small pending row is the durable recovery anchor when the process exits
    /// before a canonical meeting exists.
    pub async fn ensure_pending(
        pool: &SqlitePool,
        source_session_id: &str,
        recording_folder_hash: &str,
    ) -> Result<RecordingSessionMeetingBinding, RecordingSessionBindingStoreError> {
        validate_binding_identity(source_session_id, recording_folder_hash)?;
        let mut transaction = pool.begin().await?;
        let now = Utc::now().to_rfc3339();
        let pending_result = async {
            sqlx::query(
                r#"
                INSERT INTO recording_session_meeting_bindings (
                    source_session_id,
                    recording_folder_hash,
                    meeting_id,
                    state,
                    created_at,
                    updated_at,
                    bound_at
                ) VALUES (?, ?, NULL, 'pending', ?, ?, NULL)
                ON CONFLICT DO NOTHING
                "#,
            )
            .bind(source_session_id)
            .bind(recording_folder_hash)
            .bind(&now)
            .bind(&now)
            .execute(&mut *transaction)
            .await?;

            if let Some(binding) = sqlx::query_as::<_, RecordingSessionMeetingBinding>(
                r#"
                SELECT source_session_id, recording_folder_hash, meeting_id, state
                FROM recording_session_meeting_bindings
                WHERE source_session_id = ?
                "#,
            )
            .bind(source_session_id)
            .fetch_optional(&mut *transaction)
            .await?
            {
                if binding.recording_folder_hash != recording_folder_hash {
                    return Err(RecordingSessionBindingStoreError::SessionConflict);
                }
                return Ok(binding);
            }

            // The INSERT can be ignored because the unique folder hash belongs
            // to another session. Do not disclose either identifier.
            Err(RecordingSessionBindingStoreError::FolderConflict)
        }
        .await;

        match pending_result {
            Ok(binding) => {
                transaction.commit().await?;
                Ok(binding)
            }
            Err(error) => {
                transaction.rollback().await?;
                Err(error)
            }
        }
    }

    pub async fn load_by_session(
        pool: &SqlitePool,
        source_session_id: &str,
    ) -> Result<Option<RecordingSessionMeetingBinding>, RecordingSessionBindingStoreError> {
        validate_session_id(source_session_id)?;
        Ok(sqlx::query_as::<_, RecordingSessionMeetingBinding>(
            r#"
            SELECT source_session_id, recording_folder_hash, meeting_id, state
            FROM recording_session_meeting_bindings
            WHERE source_session_id = ?
            "#,
        )
        .bind(source_session_id)
        .fetch_optional(pool)
        .await?)
    }

    pub async fn load_by_folder_hash(
        pool: &SqlitePool,
        recording_folder_hash: &str,
    ) -> Result<Option<RecordingSessionMeetingBinding>, RecordingSessionBindingStoreError> {
        validate_folder_hash(recording_folder_hash)?;
        Ok(sqlx::query_as::<_, RecordingSessionMeetingBinding>(
            r#"
            SELECT source_session_id, recording_folder_hash, meeting_id, state
            FROM recording_session_meeting_bindings
            WHERE recording_folder_hash = ?
            "#,
        )
        .bind(recording_folder_hash)
        .fetch_optional(pool)
        .await?)
    }

    /// Complete a pending correlation inside the same transaction that creates
    /// the meeting and transcript revisions. A retry can observe AlreadyBound
    /// and return the previously committed meeting instead of duplicating it.
    pub(crate) async fn bind_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        source_session_id: &str,
        recording_folder_hash: &str,
        meeting_id: &str,
    ) -> Result<RecordingSessionBindOutcome, RecordingSessionBindingStoreError> {
        validate_binding_identity(source_session_id, recording_folder_hash)?;
        validate_meeting_id(meeting_id)?;

        let existing = sqlx::query_as::<_, RecordingSessionMeetingBinding>(
            r#"
            SELECT source_session_id, recording_folder_hash, meeting_id, state
            FROM recording_session_meeting_bindings
            WHERE source_session_id = ?
            "#,
        )
        .bind(source_session_id)
        .fetch_optional(&mut **transaction)
        .await?;

        if let Some(existing) = existing {
            if existing.recording_folder_hash != recording_folder_hash {
                return Err(RecordingSessionBindingStoreError::SessionConflict);
            }
            if let Some(existing_meeting_id) = existing.meeting_id {
                return if existing_meeting_id == meeting_id {
                    Ok(RecordingSessionBindOutcome::AlreadyBound)
                } else {
                    Err(RecordingSessionBindingStoreError::SessionConflict)
                };
            }

            let now = Utc::now().to_rfc3339();
            let updated = sqlx::query(
                r#"
                UPDATE recording_session_meeting_bindings
                SET meeting_id = ?, state = 'bound', updated_at = ?, bound_at = ?
                WHERE source_session_id = ?
                  AND recording_folder_hash = ?
                  AND state = 'pending'
                  AND meeting_id IS NULL
                "#,
            )
            .bind(meeting_id)
            .bind(&now)
            .bind(&now)
            .bind(source_session_id)
            .bind(recording_folder_hash)
            .execute(&mut **transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(RecordingSessionBindingStoreError::SessionConflict);
            }
            return Ok(RecordingSessionBindOutcome::PendingCompleted);
        }

        // A save may only complete the durable anchor created by recording
        // startup. Inserting a bound row here would allow the save path to
        // bypass that trusted lifecycle boundary.
        Err(RecordingSessionBindingStoreError::MissingPending)
    }
}

fn validate_binding_identity(
    source_session_id: &str,
    recording_folder_hash: &str,
) -> Result<(), RecordingSessionBindingStoreError> {
    validate_session_id(source_session_id)?;
    validate_folder_hash(recording_folder_hash)
}

fn validate_session_id(value: &str) -> Result<(), RecordingSessionBindingStoreError> {
    if value.is_empty() || value.len() > 256 || value.trim() != value || value.contains('\0') {
        return Err(RecordingSessionBindingStoreError::InvalidSessionId);
    }
    Ok(())
}

fn validate_meeting_id(value: &str) -> Result<(), RecordingSessionBindingStoreError> {
    if value.is_empty() || value.len() > 256 || value.trim() != value || value.contains('\0') {
        return Err(RecordingSessionBindingStoreError::SessionConflict);
    }
    Ok(())
}

fn validate_folder_hash(value: &str) -> Result<(), RecordingSessionBindingStoreError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RecordingSessionBindingStoreError::InvalidFolderHash);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    const FOLDER_ONE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const FOLDER_TWO: &str = "2222222222222222222222222222222222222222222222222222222222222222";

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

    async fn insert_meeting(pool: &SqlitePool, meeting_id: &str) {
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, 'fixture', ?, ?)",
        )
        .bind(meeting_id)
        .bind("2026-09-02T00:00:00.000Z")
        .bind("2026-09-02T00:00:00.000Z")
        .execute(pool)
        .await
        .expect("insert meeting");
    }

    #[tokio::test]
    async fn pending_binding_survives_reload_and_completes_in_meeting_transaction() {
        let pool = test_pool().await;
        let pending = RecordingSessionBindingRepository::ensure_pending(
            &pool,
            "session-recovery",
            FOLDER_ONE,
        )
        .await
        .expect("persist pending binding");
        assert_eq!(pending.state, "pending");
        assert!(pending.meeting_id.is_none());

        insert_meeting(&pool, "meeting-recovery").await;
        let mut transaction = pool.begin().await.expect("begin binding transaction");
        let outcome = RecordingSessionBindingRepository::bind_in_transaction(
            &mut transaction,
            "session-recovery",
            FOLDER_ONE,
            "meeting-recovery",
        )
        .await
        .expect("bind pending session");
        assert_eq!(outcome, RecordingSessionBindOutcome::PendingCompleted);
        transaction.commit().await.expect("commit binding");

        let recovered = RecordingSessionBindingRepository::load_by_folder_hash(&pool, FOLDER_ONE)
            .await
            .expect("reload durable binding")
            .expect("binding exists");
        assert_eq!(recovered.source_session_id, "session-recovery");
        assert_eq!(recovered.meeting_id.as_deref(), Some("meeting-recovery"));
        assert_eq!(recovered.state, "bound");
    }

    #[tokio::test]
    async fn session_and_folder_conflicts_fail_closed() {
        let pool = test_pool().await;
        RecordingSessionBindingRepository::ensure_pending(&pool, "session-one", FOLDER_ONE)
            .await
            .expect("first pending binding");
        assert!(matches!(
            RecordingSessionBindingRepository::ensure_pending(&pool, "session-one", FOLDER_TWO)
                .await,
            Err(RecordingSessionBindingStoreError::SessionConflict)
        ));
        assert!(matches!(
            RecordingSessionBindingRepository::ensure_pending(&pool, "session-two", FOLDER_ONE)
                .await,
            Err(RecordingSessionBindingStoreError::FolderConflict)
        ));
        assert!(
            RecordingSessionBindingRepository::load_by_session(&pool, "session-two")
                .await
                .expect("check atomic conflict rollback")
                .is_none()
        );
    }

    #[tokio::test]
    async fn deleting_meeting_cascades_only_the_bound_row() {
        let pool = test_pool().await;
        RecordingSessionBindingRepository::ensure_pending(&pool, "session-delete", FOLDER_ONE)
            .await
            .expect("persist start anchor");
        insert_meeting(&pool, "meeting-delete").await;
        let mut transaction = pool.begin().await.expect("begin binding transaction");
        RecordingSessionBindingRepository::bind_in_transaction(
            &mut transaction,
            "session-delete",
            FOLDER_ONE,
            "meeting-delete",
        )
        .await
        .expect("complete pending row");
        transaction.commit().await.expect("commit binding");

        sqlx::query("DELETE FROM meetings WHERE id = ?")
            .bind("meeting-delete")
            .execute(&pool)
            .await
            .expect("delete meeting");
        assert!(
            RecordingSessionBindingRepository::load_by_session(&pool, "session-delete")
                .await
                .expect("query binding")
                .is_none()
        );
    }

    #[tokio::test]
    async fn binding_without_a_pending_start_anchor_is_rejected() {
        let pool = test_pool().await;
        insert_meeting(&pool, "meeting-unanchored").await;
        let mut transaction = pool.begin().await.expect("begin binding transaction");

        assert!(matches!(
            RecordingSessionBindingRepository::bind_in_transaction(
                &mut transaction,
                "session-unanchored",
                FOLDER_ONE,
                "meeting-unanchored",
            )
            .await,
            Err(RecordingSessionBindingStoreError::MissingPending)
        ));
        transaction.rollback().await.expect("rollback transaction");
    }

    #[cfg(windows)]
    #[test]
    fn windows_folder_hash_preserves_distinct_native_utf16_paths() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use std::path::PathBuf;

        let first = PathBuf::from(OsString::from_wide(&[
            b'D' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
        ]));
        let second = PathBuf::from(OsString::from_wide(&[
            b'D' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd801,
        ]));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(
            RecordingSessionBindingRepository::recording_folder_hash(&first).unwrap(),
            RecordingSessionBindingRepository::recording_folder_hash(&second).unwrap()
        );
    }
}
