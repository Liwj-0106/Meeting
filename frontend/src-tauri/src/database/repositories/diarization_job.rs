use sqlx::{FromRow, SqlitePool};
use std::collections::HashMap;
use std::fmt;

const MAX_WINDOW_FRAMES: u64 = 90 * 48_000;
const MAX_SEGMENTS: usize = 4_096;
const MAX_SEGMENT_TEXT_CHARS: usize = 16_384;
const MAX_TOTAL_TEXT_CHARS: usize = 1_048_576;

pub struct DiarizationJobRepository;

#[derive(Debug, thiserror::Error)]
pub enum DiarizationJobStoreError {
    #[error("invalid diarization job field: {0}")]
    InvalidInput(&'static str),
    #[error("diarization job was not found")]
    NotFound,
    #[error("diarization job status transition was rejected")]
    InvalidTransition,
    #[error("an immutable diarization {0} already exists with different content")]
    ImmutableConflict(&'static str),
    #[error("a diarization numeric field exceeds SQLite's signed integer range")]
    IntegerOutOfRange,
    #[error("stored diarization state is invalid: {0}")]
    InvalidStoredState(&'static str),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiarizationJobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl DiarizationJobStatus {
    fn parse(value: &str) -> Result<Self, DiarizationJobStoreError> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(DiarizationJobStoreError::InvalidStoredState("status")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiarizationJobErrorCode {
    InvalidConfiguration,
    RuntimeUnavailable,
    ModelNotInstalled,
    InvalidRequest,
    AudioUnavailable,
    AudioOutsideRoot,
    ModelRevisionMismatch,
    QueueFull,
    WorkerUnavailable,
    CircuitOpen,
    StartupTimeout,
    HandshakeRejected,
    WorkerCrashed,
    JobTimeout,
    JobCancelled,
    InvalidResponse,
    CancellationFailed,
    SupervisorStopping,
    SupervisorStopped,
    ShutdownTimeout,
    ShutdownFailed,
}

impl DiarizationJobErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::ModelNotInstalled => "model_not_installed",
            Self::InvalidRequest => "invalid_request",
            Self::AudioUnavailable => "audio_unavailable",
            Self::AudioOutsideRoot => "audio_outside_root",
            Self::ModelRevisionMismatch => "model_revision_mismatch",
            Self::QueueFull => "queue_full",
            Self::WorkerUnavailable => "worker_unavailable",
            Self::CircuitOpen => "circuit_open",
            Self::StartupTimeout => "startup_timeout",
            Self::HandshakeRejected => "handshake_rejected",
            Self::WorkerCrashed => "worker_crashed",
            Self::JobTimeout => "job_timeout",
            Self::JobCancelled => "job_cancelled",
            Self::InvalidResponse => "invalid_response",
            Self::CancellationFailed => "cancellation_failed",
            Self::SupervisorStopping => "supervisor_stopping",
            Self::SupervisorStopped => "supervisor_stopped",
            Self::ShutdownTimeout => "shutdown_timeout",
            Self::ShutdownFailed => "shutdown_failed",
        }
    }

    fn parse(value: &str) -> Result<Self, DiarizationJobStoreError> {
        match value {
            "invalid_configuration" => Ok(Self::InvalidConfiguration),
            "runtime_unavailable" => Ok(Self::RuntimeUnavailable),
            "model_not_installed" => Ok(Self::ModelNotInstalled),
            "invalid_request" => Ok(Self::InvalidRequest),
            "audio_unavailable" => Ok(Self::AudioUnavailable),
            "audio_outside_root" => Ok(Self::AudioOutsideRoot),
            "model_revision_mismatch" => Ok(Self::ModelRevisionMismatch),
            "queue_full" => Ok(Self::QueueFull),
            "worker_unavailable" => Ok(Self::WorkerUnavailable),
            "circuit_open" => Ok(Self::CircuitOpen),
            "startup_timeout" => Ok(Self::StartupTimeout),
            "handshake_rejected" => Ok(Self::HandshakeRejected),
            "worker_crashed" => Ok(Self::WorkerCrashed),
            "job_timeout" => Ok(Self::JobTimeout),
            "job_cancelled" => Ok(Self::JobCancelled),
            "invalid_response" => Ok(Self::InvalidResponse),
            "cancellation_failed" => Ok(Self::CancellationFailed),
            "supervisor_stopping" => Ok(Self::SupervisorStopping),
            "supervisor_stopped" => Ok(Self::SupervisorStopped),
            "shutdown_timeout" => Ok(Self::ShutdownTimeout),
            "shutdown_failed" => Ok(Self::ShutdownFailed),
            _ => Err(DiarizationJobStoreError::InvalidStoredState("error_code")),
        }
    }

    pub const fn from_worker_code(value: crate::audio::diarization::MossWorkerErrorCode) -> Self {
        use crate::audio::diarization::MossWorkerErrorCode;
        match value {
            MossWorkerErrorCode::InvalidConfiguration => Self::InvalidConfiguration,
            MossWorkerErrorCode::RuntimeUnavailable => Self::RuntimeUnavailable,
            MossWorkerErrorCode::ModelNotInstalled => Self::ModelNotInstalled,
            MossWorkerErrorCode::InvalidRequest => Self::InvalidRequest,
            MossWorkerErrorCode::AudioUnavailable => Self::AudioUnavailable,
            MossWorkerErrorCode::AudioOutsideRoot => Self::AudioOutsideRoot,
            MossWorkerErrorCode::ModelRevisionMismatch => Self::ModelRevisionMismatch,
            MossWorkerErrorCode::QueueFull => Self::QueueFull,
            MossWorkerErrorCode::WorkerUnavailable => Self::WorkerUnavailable,
            MossWorkerErrorCode::CircuitOpen => Self::CircuitOpen,
            MossWorkerErrorCode::StartupTimeout => Self::StartupTimeout,
            MossWorkerErrorCode::HandshakeRejected => Self::HandshakeRejected,
            MossWorkerErrorCode::WorkerCrashed => Self::WorkerCrashed,
            MossWorkerErrorCode::JobTimeout => Self::JobTimeout,
            MossWorkerErrorCode::JobCancelled => Self::JobCancelled,
            MossWorkerErrorCode::InvalidResponse => Self::InvalidResponse,
            MossWorkerErrorCode::CancellationFailed => Self::CancellationFailed,
            MossWorkerErrorCode::SupervisorStopping => Self::SupervisorStopping,
            MossWorkerErrorCode::SupervisorStopped => Self::SupervisorStopped,
            MossWorkerErrorCode::ShutdownTimeout => Self::ShutdownTimeout,
            MossWorkerErrorCode::ShutdownFailed => Self::ShutdownFailed,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct NewDiarizationJob {
    pub job_id: String,
    pub meeting_id: String,
    pub session_id: String,
    pub window_id: String,
    pub window_start_frame: u64,
    pub window_end_frame: u64,
    pub model_revision: String,
    pub created_at: String,
}

impl fmt::Debug for NewDiarizationJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewDiarizationJob")
            .field("job_id", &self.job_id)
            .field("meeting_id", &self.meeting_id)
            .field("session_id", &self.session_id)
            .field("window_id", &self.window_id)
            .field("window_start_frame", &self.window_start_frame)
            .field("window_end_frame", &self.window_end_frame)
            .field("model_revision", &self.model_revision)
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiarizationJobRecord {
    pub job_id: String,
    pub meeting_id: String,
    pub session_id: String,
    pub window_id: String,
    pub window_start_frame: u64,
    pub window_end_frame: u64,
    pub model_revision: String,
    pub status: DiarizationJobStatus,
    pub error_code: Option<DiarizationJobErrorCode>,
    pub attempt_count: u64,
    pub latest_result_revision: Option<u64>,
    pub latest_result_revision_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiarizationResultStatus {
    Succeeded,
    Failed,
    Cancelled,
}

impl DiarizationResultStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, DiarizationJobStoreError> {
        match value {
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(DiarizationJobStoreError::InvalidStoredState(
                "result status",
            )),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DiarizationResultSegment {
    pub start_frame: u64,
    pub end_frame: u64,
    pub speaker: String,
    pub transcript: String,
}

impl fmt::Debug for DiarizationResultSegment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiarizationResultSegment")
            .field("start_frame", &self.start_frame)
            .field("end_frame", &self.end_frame)
            .field("speaker", &self.speaker)
            .field(
                "transcript",
                &format!("<redacted:{} chars>", self.transcript.chars().count()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DiarizationResultRevision {
    pub result_revision_id: String,
    pub job_id: String,
    pub meeting_id: String,
    pub revision: u64,
    pub window_id: String,
    pub window_start_frame: u64,
    pub window_end_frame: u64,
    pub model_revision: String,
    pub status: DiarizationResultStatus,
    pub error_code: Option<DiarizationJobErrorCode>,
    pub segments: Vec<DiarizationResultSegment>,
    pub created_at: String,
}

impl fmt::Debug for DiarizationResultRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiarizationResultRevision")
            .field("result_revision_id", &self.result_revision_id)
            .field("job_id", &self.job_id)
            .field("meeting_id", &self.meeting_id)
            .field("revision", &self.revision)
            .field("window_id", &self.window_id)
            .field("window_start_frame", &self.window_start_frame)
            .field("window_end_frame", &self.window_end_frame)
            .field("model_revision", &self.model_revision)
            .field("status", &self.status)
            .field("error_code", &self.error_code)
            .field("segment_count", &self.segments.len())
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[derive(FromRow)]
struct JobRow {
    job_id: String,
    meeting_id: String,
    session_id: String,
    window_id: String,
    window_start_frame: i64,
    window_end_frame: i64,
    model_revision: String,
    status: String,
    error_code: Option<String>,
    attempt_count: i64,
    latest_result_revision: Option<i64>,
    latest_result_revision_id: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(FromRow)]
struct ResultRow {
    result_revision_id: String,
    job_id: String,
    meeting_id: String,
    revision: i64,
    window_id: String,
    window_start_frame: i64,
    window_end_frame: i64,
    model_revision: String,
    status: String,
    error_code: Option<String>,
    created_at: String,
}

#[derive(FromRow)]
struct SegmentRow {
    segment_index: i64,
    start_frame: i64,
    end_frame: i64,
    speaker: String,
    transcript: String,
}

impl DiarizationJobRepository {
    pub async fn create(
        pool: &SqlitePool,
        job: &NewDiarizationJob,
    ) -> Result<(), DiarizationJobStoreError> {
        validate_job(job)?;
        let mut transaction = pool.begin().await?;
        let insert = sqlx::query(
            r#"
            INSERT INTO diarization_jobs (
                job_id, meeting_id, session_id, window_id,
                window_start_frame, window_end_frame, model_revision,
                status, error_code, attempt_count,
                latest_result_revision, latest_result_revision_id,
                created_at, updated_at
            )
            SELECT ?, ?, ?, ?, ?, ?, ?, 'queued', NULL, 0, NULL, NULL, ?, ?
            WHERE NOT EXISTS (
                SELECT 1 FROM diarization_jobs WHERE job_id = ?
            )
            ON CONFLICT(job_id) DO NOTHING
            "#,
        )
        .bind(&job.job_id)
        .bind(&job.meeting_id)
        .bind(&job.session_id)
        .bind(&job.window_id)
        .bind(sqlite_integer(job.window_start_frame)?)
        .bind(sqlite_integer(job.window_end_frame)?)
        .bind(&job.model_revision)
        .bind(&job.created_at)
        .bind(&job.created_at)
        .bind(&job.job_id)
        .execute(&mut *transaction)
        .await?;
        if insert.rows_affected() == 0 {
            let existing = sqlx::query_as::<_, JobRow>(
                r#"
                SELECT
                    job_id, meeting_id, session_id, window_id,
                    window_start_frame, window_end_frame, model_revision,
                    status, error_code, attempt_count,
                    latest_result_revision, latest_result_revision_id,
                    created_at, updated_at
                FROM diarization_jobs
                WHERE job_id = ?
                "#,
            )
            .bind(&job.job_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(DiarizationJobStoreError::ImmutableConflict("job"))?;
            let existing = job_from_row(existing)?;
            if !job_identity_matches(&existing, job) {
                transaction.rollback().await?;
                return Err(DiarizationJobStoreError::ImmutableConflict("job"));
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn mark_running(
        pool: &SqlitePool,
        meeting_id: &str,
        job_id: &str,
        updated_at: &str,
    ) -> Result<(), DiarizationJobStoreError> {
        validate_identifier("meeting_id", meeting_id)?;
        validate_identifier("job_id", job_id)?;
        validate_timestamp(updated_at)?;
        let result = sqlx::query(
            r#"
            UPDATE diarization_jobs
            SET status = 'running',
                error_code = NULL,
                attempt_count = CASE
                    WHEN status = 'queued' THEN attempt_count + 1
                    ELSE attempt_count
                END,
                updated_at = ?
            WHERE meeting_id = ?
              AND job_id = ?
              AND status IN ('queued', 'running')
            "#,
        )
        .bind(updated_at)
        .bind(meeting_id)
        .bind(job_id)
        .execute(pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DiarizationJobStoreError::InvalidTransition);
        }
        Ok(())
    }

    pub async fn requeue_interrupted(
        pool: &SqlitePool,
        meeting_id: &str,
        updated_at: &str,
    ) -> Result<u64, DiarizationJobStoreError> {
        validate_identifier("meeting_id", meeting_id)?;
        validate_timestamp(updated_at)?;
        let result = sqlx::query(
            r#"
            UPDATE diarization_jobs
            SET status = 'queued', error_code = NULL, updated_at = ?
            WHERE meeting_id = ? AND status = 'running'
            "#,
        )
        .bind(updated_at)
        .bind(meeting_id)
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn append_result(
        pool: &SqlitePool,
        result: &DiarizationResultRevision,
    ) -> Result<(), DiarizationJobStoreError> {
        validate_result(result)?;
        let mut transaction = pool.begin().await?;
        let inserted =
            async {
                let insert = sqlx::query(
                    r#"
                INSERT INTO diarization_result_revisions (
                    result_revision_id, job_id, meeting_id, revision,
                    window_id, window_start_frame, window_end_frame,
                    model_revision, status, error_code, created_at
                )
                SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM diarization_result_revisions
                    WHERE result_revision_id = ?
                       OR (job_id = ? AND revision = ?)
                )
                ON CONFLICT DO NOTHING
                "#,
                )
                .bind(&result.result_revision_id)
                .bind(&result.job_id)
                .bind(&result.meeting_id)
                .bind(sqlite_integer(result.revision)?)
                .bind(&result.window_id)
                .bind(sqlite_integer(result.window_start_frame)?)
                .bind(sqlite_integer(result.window_end_frame)?)
                .bind(&result.model_revision)
                .bind(result.status.as_str())
                .bind(result.error_code.map(DiarizationJobErrorCode::as_str))
                .bind(&result.created_at)
                .bind(&result.result_revision_id)
                .bind(&result.job_id)
                .bind(sqlite_integer(result.revision)?)
                .execute(&mut *transaction)
                .await?;

                if insert.rows_affected() == 1 {
                    for (index, segment) in result.segments.iter().enumerate() {
                        sqlx::query(
                            r#"
                        INSERT INTO diarization_result_segments (
                            result_revision_id, segment_index,
                            start_frame, end_frame, speaker, transcript
                        ) VALUES (?, ?, ?, ?, ?, ?)
                        "#,
                        )
                        .bind(&result.result_revision_id)
                        .bind(
                            i64::try_from(index)
                                .map_err(|_| DiarizationJobStoreError::IntegerOutOfRange)?,
                        )
                        .bind(sqlite_integer(segment.start_frame)?)
                        .bind(sqlite_integer(segment.end_frame)?)
                        .bind(&segment.speaker)
                        .bind(&segment.transcript)
                        .execute(&mut *transaction)
                        .await?;
                    }
                } else {
                    let rows = sqlx::query_as::<_, ResultRow>(
                        r#"
                    SELECT
                        result_revision_id, job_id, meeting_id, revision,
                        window_id, window_start_frame, window_end_frame,
                        model_revision, status, error_code, created_at
                    FROM diarization_result_revisions
                    WHERE result_revision_id = ?
                       OR (job_id = ? AND revision = ?)
                    ORDER BY result_revision_id
                    "#,
                    )
                    .bind(&result.result_revision_id)
                    .bind(&result.job_id)
                    .bind(sqlite_integer(result.revision)?)
                    .fetch_all(&mut *transaction)
                    .await?;
                    if rows.len() != 1 {
                        return Err(DiarizationJobStoreError::ImmutableConflict(
                            "result revision",
                        ));
                    }
                    let row = rows.into_iter().next().ok_or(
                        DiarizationJobStoreError::ImmutableConflict("result revision"),
                    )?;
                    let segment_rows = sqlx::query_as::<_, SegmentRow>(
                        r#"
                    SELECT segment_index, start_frame, end_frame, speaker, transcript
                    FROM diarization_result_segments
                    WHERE result_revision_id = ?
                    ORDER BY segment_index
                    "#,
                    )
                    .bind(&row.result_revision_id)
                    .fetch_all(&mut *transaction)
                    .await?;
                    let existing = result_from_row(row, segments_from_rows(segment_rows)?)?;
                    if existing != *result {
                        return Err(DiarizationJobStoreError::ImmutableConflict(
                            "result revision",
                        ));
                    }
                }
                Ok::<(), DiarizationJobStoreError>(())
            }
            .await;

        match inserted {
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

    pub async fn get(
        pool: &SqlitePool,
        meeting_id: &str,
        job_id: &str,
    ) -> Result<DiarizationJobRecord, DiarizationJobStoreError> {
        let row = sqlx::query_as::<_, JobRow>(
            r#"
            SELECT
                job_id, meeting_id, session_id, window_id,
                window_start_frame, window_end_frame, model_revision,
                status, error_code, attempt_count,
                latest_result_revision, latest_result_revision_id,
                created_at, updated_at
            FROM diarization_jobs
            WHERE meeting_id = ? AND job_id = ?
            "#,
        )
        .bind(meeting_id)
        .bind(job_id)
        .fetch_optional(pool)
        .await?
        .ok_or(DiarizationJobStoreError::NotFound)?;
        job_from_row(row)
    }

    pub async fn list_recoverable(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Vec<DiarizationJobRecord>, DiarizationJobStoreError> {
        let rows = sqlx::query_as::<_, JobRow>(
            r#"
            SELECT
                job_id, meeting_id, session_id, window_id,
                window_start_frame, window_end_frame, model_revision,
                status, error_code, attempt_count,
                latest_result_revision, latest_result_revision_id,
                created_at, updated_at
            FROM diarization_jobs
            WHERE meeting_id = ? AND status IN ('queued', 'running')
            ORDER BY created_at, job_id
            "#,
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await?;
        rows.into_iter().map(job_from_row).collect()
    }

    pub async fn load_latest_result(
        pool: &SqlitePool,
        meeting_id: &str,
        job_id: &str,
    ) -> Result<Option<DiarizationResultRevision>, DiarizationJobStoreError> {
        let row = sqlx::query_as::<_, ResultRow>(
            r#"
            SELECT
                revisions.result_revision_id,
                revisions.job_id,
                revisions.meeting_id,
                revisions.revision,
                revisions.window_id,
                revisions.window_start_frame,
                revisions.window_end_frame,
                revisions.model_revision,
                revisions.status,
                revisions.error_code,
                revisions.created_at
            FROM diarization_jobs AS jobs
            JOIN diarization_result_revisions AS revisions
              ON revisions.result_revision_id = jobs.latest_result_revision_id
            WHERE jobs.meeting_id = ? AND jobs.job_id = ?
            "#,
        )
        .bind(meeting_id)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let segments = sqlx::query_as::<_, SegmentRow>(
            r#"
            SELECT segment_index, start_frame, end_frame, speaker, transcript
            FROM diarization_result_segments
            WHERE result_revision_id = ?
            ORDER BY segment_index
            "#,
        )
        .bind(&row.result_revision_id)
        .fetch_all(pool)
        .await?;
        let segments = segments_from_rows(segments)?;
        Ok(Some(result_from_row(row, segments)?))
    }
}

fn job_identity_matches(existing: &DiarizationJobRecord, requested: &NewDiarizationJob) -> bool {
    existing.job_id == requested.job_id
        && existing.meeting_id == requested.meeting_id
        && existing.session_id == requested.session_id
        && existing.window_id == requested.window_id
        && existing.window_start_frame == requested.window_start_frame
        && existing.window_end_frame == requested.window_end_frame
        && existing.model_revision == requested.model_revision
        && existing.created_at == requested.created_at
}

fn validate_job(job: &NewDiarizationJob) -> Result<(), DiarizationJobStoreError> {
    validate_identifier("job_id", &job.job_id)?;
    validate_identifier("meeting_id", &job.meeting_id)?;
    validate_identifier("session_id", &job.session_id)?;
    validate_identifier("window_id", &job.window_id)?;
    validate_window(job.window_start_frame, job.window_end_frame)?;
    validate_revision(&job.model_revision)?;
    validate_timestamp(&job.created_at)
}

fn validate_result(result: &DiarizationResultRevision) -> Result<(), DiarizationJobStoreError> {
    validate_identifier("result_revision_id", &result.result_revision_id)?;
    validate_identifier("job_id", &result.job_id)?;
    validate_identifier("meeting_id", &result.meeting_id)?;
    validate_identifier("window_id", &result.window_id)?;
    if result.revision == 0 {
        return Err(DiarizationJobStoreError::InvalidInput("revision"));
    }
    validate_window(result.window_start_frame, result.window_end_frame)?;
    validate_revision(&result.model_revision)?;
    validate_timestamp(&result.created_at)?;
    match (result.status, result.error_code) {
        (DiarizationResultStatus::Succeeded, None) => {}
        (DiarizationResultStatus::Failed, Some(code))
            if code != DiarizationJobErrorCode::JobCancelled => {}
        (DiarizationResultStatus::Cancelled, Some(DiarizationJobErrorCode::JobCancelled)) => {}
        _ => return Err(DiarizationJobStoreError::InvalidInput("status/error_code")),
    }
    if result.status == DiarizationResultStatus::Succeeded {
        validate_segments(
            &result.segments,
            result.window_start_frame,
            result.window_end_frame,
        )?;
    } else if !result.segments.is_empty() {
        return Err(DiarizationJobStoreError::InvalidInput("segments"));
    }
    Ok(())
}

fn validate_segments(
    segments: &[DiarizationResultSegment],
    window_start: u64,
    window_end: u64,
) -> Result<(), DiarizationJobStoreError> {
    if segments.len() > MAX_SEGMENTS {
        return Err(DiarizationJobStoreError::InvalidInput("segments"));
    }
    let mut total_chars = 0usize;
    let mut previous = None;
    let mut last_end_by_speaker = HashMap::new();
    for segment in segments {
        if segment.start_frame < window_start
            || segment.end_frame > window_end
            || segment.end_frame <= segment.start_frame
        {
            return Err(DiarizationJobStoreError::InvalidInput("segment range"));
        }
        validate_speaker(&segment.speaker)?;
        let chars = segment.transcript.chars().count();
        if chars == 0
            || chars > MAX_SEGMENT_TEXT_CHARS
            || segment
                .transcript
                .chars()
                .any(|character| character.is_control())
        {
            return Err(DiarizationJobStoreError::InvalidInput("segment transcript"));
        }
        total_chars = total_chars.saturating_add(chars);
        if total_chars > MAX_TOTAL_TEXT_CHARS {
            return Err(DiarizationJobStoreError::InvalidInput("segment transcript"));
        }
        let key = (
            segment.start_frame,
            segment.end_frame,
            segment.speaker.as_str(),
        );
        if previous.is_some_and(|prior| prior > key) {
            return Err(DiarizationJobStoreError::InvalidInput("segment order"));
        }
        previous = Some(key);
        if last_end_by_speaker
            .get(segment.speaker.as_str())
            .is_some_and(|last_end| segment.start_frame < *last_end)
        {
            return Err(DiarizationJobStoreError::InvalidInput(
                "segment speaker overlap",
            ));
        }
        last_end_by_speaker.insert(segment.speaker.as_str(), segment.end_frame);
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), DiarizationJobStoreError> {
    let valid = !value.is_empty()
        && value.chars().count() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character));
    if !valid {
        return Err(DiarizationJobStoreError::InvalidInput(field));
    }
    Ok(())
}

fn validate_revision(value: &str) -> Result<(), DiarizationJobStoreError> {
    if value.is_empty()
        || value.chars().count() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character))
    {
        return Err(DiarizationJobStoreError::InvalidInput("model_revision"));
    }
    Ok(())
}

fn validate_speaker(value: &str) -> Result<(), DiarizationJobStoreError> {
    if value.is_empty()
        || value.chars().count() > 128
        || value.chars().any(|character| character.is_control())
    {
        return Err(DiarizationJobStoreError::InvalidInput("speaker"));
    }
    Ok(())
}

fn validate_window(start: u64, end: u64) -> Result<(), DiarizationJobStoreError> {
    if end <= start || end.saturating_sub(start) > MAX_WINDOW_FRAMES {
        return Err(DiarizationJobStoreError::InvalidInput("window"));
    }
    Ok(())
}

fn validate_timestamp(value: &str) -> Result<(), DiarizationJobStoreError> {
    if value.is_empty()
        || value.chars().count() > 64
        || value.chars().any(|character| character.is_control())
    {
        return Err(DiarizationJobStoreError::InvalidInput("timestamp"));
    }
    Ok(())
}

fn sqlite_integer(value: u64) -> Result<i64, DiarizationJobStoreError> {
    i64::try_from(value).map_err(|_| DiarizationJobStoreError::IntegerOutOfRange)
}

fn unsigned(value: i64, field: &'static str) -> Result<u64, DiarizationJobStoreError> {
    u64::try_from(value).map_err(|_| DiarizationJobStoreError::InvalidStoredState(field))
}

fn job_from_row(row: JobRow) -> Result<DiarizationJobRecord, DiarizationJobStoreError> {
    Ok(DiarizationJobRecord {
        job_id: row.job_id,
        meeting_id: row.meeting_id,
        session_id: row.session_id,
        window_id: row.window_id,
        window_start_frame: unsigned(row.window_start_frame, "window_start_frame")?,
        window_end_frame: unsigned(row.window_end_frame, "window_end_frame")?,
        model_revision: row.model_revision,
        status: DiarizationJobStatus::parse(&row.status)?,
        error_code: row
            .error_code
            .as_deref()
            .map(DiarizationJobErrorCode::parse)
            .transpose()?,
        attempt_count: unsigned(row.attempt_count, "attempt_count")?,
        latest_result_revision: row
            .latest_result_revision
            .map(|value| unsigned(value, "latest_result_revision"))
            .transpose()?,
        latest_result_revision_id: row.latest_result_revision_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn segment_from_row(row: SegmentRow) -> Result<DiarizationResultSegment, DiarizationJobStoreError> {
    Ok(DiarizationResultSegment {
        start_frame: unsigned(row.start_frame, "segment start_frame")?,
        end_frame: unsigned(row.end_frame, "segment end_frame")?,
        speaker: row.speaker,
        transcript: row.transcript,
    })
}

fn segments_from_rows(
    rows: Vec<SegmentRow>,
) -> Result<Vec<DiarizationResultSegment>, DiarizationJobStoreError> {
    rows.into_iter()
        .enumerate()
        .map(|(expected_index, row)| {
            if row.segment_index
                != i64::try_from(expected_index)
                    .map_err(|_| DiarizationJobStoreError::IntegerOutOfRange)?
            {
                return Err(DiarizationJobStoreError::InvalidStoredState(
                    "segment_index",
                ));
            }
            segment_from_row(row)
        })
        .collect()
}

fn result_from_row(
    row: ResultRow,
    segments: Vec<DiarizationResultSegment>,
) -> Result<DiarizationResultRevision, DiarizationJobStoreError> {
    Ok(DiarizationResultRevision {
        result_revision_id: row.result_revision_id,
        job_id: row.job_id,
        meeting_id: row.meeting_id,
        revision: unsigned(row.revision, "result revision")?,
        window_id: row.window_id,
        window_start_frame: unsigned(row.window_start_frame, "window_start_frame")?,
        window_end_frame: unsigned(row.window_end_frame, "window_end_frame")?,
        model_revision: row.model_revision,
        status: DiarizationResultStatus::parse(&row.status)?,
        error_code: row
            .error_code
            .as_deref()
            .map(DiarizationJobErrorCode::parse)
            .transpose()?,
        segments,
        created_at: row.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory SQLite");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run fresh migrations");
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind("meeting-fixture")
            .bind("Synthetic")
            .bind("2026-09-02T00:00:00Z")
            .bind("2026-09-02T00:00:00Z")
            .execute(&pool)
            .await
            .expect("insert meeting fixture");
        pool
    }

    fn job(id: &str, window_id: &str) -> NewDiarizationJob {
        NewDiarizationJob {
            job_id: id.to_string(),
            meeting_id: "meeting-fixture".to_string(),
            session_id: "session-fixture".to_string(),
            window_id: window_id.to_string(),
            window_start_frame: 0,
            window_end_frame: 48_000,
            model_revision: "fixture-model-v1".to_string(),
            created_at: "2026-09-02T00:00:00Z".to_string(),
        }
    }

    fn result(
        job_id: &str,
        window_id: &str,
        revision: u64,
        status: DiarizationResultStatus,
        error_code: Option<DiarizationJobErrorCode>,
    ) -> DiarizationResultRevision {
        DiarizationResultRevision {
            result_revision_id: format!("result-{job_id}-{revision}"),
            job_id: job_id.to_string(),
            meeting_id: "meeting-fixture".to_string(),
            revision,
            window_id: window_id.to_string(),
            window_start_frame: 0,
            window_end_frame: 48_000,
            model_revision: "fixture-model-v1".to_string(),
            status,
            error_code,
            segments: if status == DiarizationResultStatus::Succeeded {
                vec![DiarizationResultSegment {
                    start_frame: 0,
                    end_frame: 4_800,
                    speaker: "S01".to_string(),
                    transcript: "synthetic fixture".to_string(),
                }]
            } else {
                Vec::new()
            },
            created_at: format!("2026-09-02T00:00:0{revision}Z"),
        }
    }

    #[tokio::test]
    async fn fresh_database_persists_jobs_and_never_rolls_latest_result_back() {
        let pool = fresh_pool().await;
        DiarizationJobRepository::create(&pool, &job("job-one", "window-one"))
            .await
            .expect("create job");
        DiarizationJobRepository::mark_running(
            &pool,
            "meeting-fixture",
            "job-one",
            "2026-09-02T00:00:01Z",
        )
        .await
        .expect("mark running");

        DiarizationJobRepository::append_result(
            &pool,
            &result(
                "job-one",
                "window-one",
                3,
                DiarizationResultStatus::Succeeded,
                None,
            ),
        )
        .await
        .expect("append newest result");
        DiarizationJobRepository::append_result(
            &pool,
            &result(
                "job-one",
                "window-one",
                2,
                DiarizationResultStatus::Failed,
                Some(DiarizationJobErrorCode::WorkerCrashed),
            ),
        )
        .await
        .expect("append late older result");

        let stored = DiarizationJobRepository::get(&pool, "meeting-fixture", "job-one")
            .await
            .expect("load job");
        assert_eq!(stored.status, DiarizationJobStatus::Succeeded);
        assert_eq!(stored.attempt_count, 1);
        assert_eq!(stored.latest_result_revision, Some(3));
        assert_eq!(stored.error_code, None);

        let latest =
            DiarizationJobRepository::load_latest_result(&pool, "meeting-fixture", "job-one")
                .await
                .expect("load latest result")
                .expect("latest result exists");
        assert_eq!(latest.revision, 3);
        assert_eq!(latest.segments.len(), 1);
    }

    #[tokio::test]
    async fn exact_job_and_result_retries_are_idempotent_after_commit() {
        let pool = fresh_pool().await;
        let requested_job = job("job-retry", "window-retry");
        DiarizationJobRepository::create(&pool, &requested_job)
            .await
            .expect("create job");
        DiarizationJobRepository::mark_running(
            &pool,
            "meeting-fixture",
            "job-retry",
            "2026-09-02T00:00:01Z",
        )
        .await
        .expect("mark running");
        DiarizationJobRepository::create(&pool, &requested_job)
            .await
            .expect("exact job retry after state change");

        let mut committed = result(
            "job-retry",
            "window-retry",
            1,
            DiarizationResultStatus::Succeeded,
            None,
        );
        committed.segments.push(DiarizationResultSegment {
            start_frame: 4_800,
            end_frame: 9_600,
            speaker: "S01".to_string(),
            transcript: "second synthetic segment".to_string(),
        });
        DiarizationJobRepository::append_result(&pool, &committed)
            .await
            .expect("append result");
        DiarizationJobRepository::append_result(&pool, &committed)
            .await
            .expect("exact result retry after commit");

        let stored = DiarizationJobRepository::get(&pool, "meeting-fixture", "job-retry")
            .await
            .expect("load retried job");
        assert_eq!(stored.attempt_count, 1);
        assert_eq!(stored.latest_result_revision, Some(1));
        let persisted =
            DiarizationJobRepository::load_latest_result(&pool, "meeting-fixture", "job-retry")
                .await
                .expect("load retried result")
                .expect("retried result exists");
        assert_eq!(persisted, committed);
        let result_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM diarization_result_revisions WHERE job_id = ?",
        )
        .bind("job-retry")
        .fetch_one(&pool)
        .await
        .expect("count result revisions");
        let segment_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM diarization_result_segments WHERE result_revision_id = ?",
        )
        .bind(&committed.result_revision_id)
        .fetch_one(&pool)
        .await
        .expect("count result segments");
        assert_eq!(result_count, 1);
        assert_eq!(segment_count, 2);
    }

    #[tokio::test]
    async fn changed_immutable_retries_fail_without_mutating_latest_result() {
        let pool = fresh_pool().await;
        let requested_job = job("job-conflict", "window-conflict");
        DiarizationJobRepository::create(&pool, &requested_job)
            .await
            .expect("create job");

        let mut changed_job = requested_job.clone();
        changed_job.session_id = "different-session".to_string();
        assert!(matches!(
            DiarizationJobRepository::create(&pool, &changed_job).await,
            Err(DiarizationJobStoreError::ImmutableConflict("job"))
        ));

        let committed = result(
            "job-conflict",
            "window-conflict",
            1,
            DiarizationResultStatus::Succeeded,
            None,
        );
        DiarizationJobRepository::append_result(&pool, &committed)
            .await
            .expect("append immutable result");

        let mut changed_payload = committed.clone();
        changed_payload.segments[0].transcript = "different synthetic text".to_string();
        assert!(matches!(
            DiarizationJobRepository::append_result(&pool, &changed_payload).await,
            Err(DiarizationJobStoreError::ImmutableConflict(
                "result revision"
            ))
        ));

        let mut reused_revision = committed.clone();
        reused_revision.result_revision_id = "different-result-id".to_string();
        assert!(matches!(
            DiarizationJobRepository::append_result(&pool, &reused_revision).await,
            Err(DiarizationJobStoreError::ImmutableConflict(
                "result revision"
            ))
        ));

        let latest =
            DiarizationJobRepository::load_latest_result(&pool, "meeting-fixture", "job-conflict")
                .await
                .expect("load latest result")
                .expect("latest result exists");
        assert_eq!(latest, committed);
        let result_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM diarization_result_revisions WHERE job_id = ?",
        )
        .bind("job-conflict")
        .fetch_one(&pool)
        .await
        .expect("count immutable revisions");
        assert_eq!(result_count, 1);
    }

    #[tokio::test]
    async fn interrupted_running_jobs_are_requeued_without_losing_attempt_count() {
        let pool = fresh_pool().await;
        DiarizationJobRepository::create(&pool, &job("job-recovery", "window-recovery"))
            .await
            .expect("create job");
        DiarizationJobRepository::mark_running(
            &pool,
            "meeting-fixture",
            "job-recovery",
            "2026-09-02T00:00:01Z",
        )
        .await
        .expect("mark running");
        assert_eq!(
            DiarizationJobRepository::requeue_interrupted(
                &pool,
                "meeting-fixture",
                "2026-09-02T00:00:02Z",
            )
            .await
            .expect("requeue interrupted"),
            1
        );
        let recoverable = DiarizationJobRepository::list_recoverable(&pool, "meeting-fixture")
            .await
            .expect("list recoverable");
        assert_eq!(recoverable.len(), 1);
        assert_eq!(recoverable[0].status, DiarizationJobStatus::Queued);
        assert_eq!(recoverable[0].attempt_count, 1);
    }

    #[tokio::test]
    async fn schema_rejects_raw_error_strings_and_cross_window_results() {
        let pool = fresh_pool().await;
        DiarizationJobRepository::create(&pool, &job("job-guard", "window-guard"))
            .await
            .expect("create job");
        let raw_error = sqlx::query(
            "UPDATE diarization_jobs SET status = 'failed', error_code = ? WHERE job_id = ?",
        )
        .bind("token=must-not-be-stored")
        .bind("job-guard")
        .execute(&pool)
        .await;
        assert!(raw_error.is_err());

        let mismatched = result(
            "job-guard",
            "another-window",
            1,
            DiarizationResultStatus::Succeeded,
            None,
        );
        assert!(DiarizationJobRepository::append_result(&pool, &mismatched)
            .await
            .is_err());
        assert!(DiarizationJobRepository::load_latest_result(
            &pool,
            "meeting-fixture",
            "job-guard"
        )
        .await
        .expect("load latest after rollback")
        .is_none());
    }

    #[test]
    fn debug_output_redacts_transcript_text() {
        let value = result(
            "job-debug",
            "window-debug",
            1,
            DiarizationResultStatus::Succeeded,
            None,
        );
        let debug = format!("{:?}", value.segments[0]);
        assert!(!debug.contains("synthetic fixture"));
        assert!(debug.contains("<redacted:"));
    }
}
