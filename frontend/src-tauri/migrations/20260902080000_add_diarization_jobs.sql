-- Durable MOSS diarization jobs and immutable, versioned worker results.
-- Worker stderr and arbitrary exception strings have no storage column.

CREATE TABLE IF NOT EXISTS diarization_jobs (
    job_id TEXT PRIMARY KEY NOT NULL CHECK (length(job_id) BETWEEN 1 AND 128),
    meeting_id TEXT NOT NULL CHECK (length(meeting_id) BETWEEN 1 AND 128),
    session_id TEXT NOT NULL CHECK (length(session_id) BETWEEN 1 AND 128),
    window_id TEXT NOT NULL CHECK (length(window_id) BETWEEN 1 AND 128),
    window_start_frame INTEGER NOT NULL CHECK (window_start_frame >= 0),
    window_end_frame INTEGER NOT NULL CHECK (window_end_frame > window_start_frame),
    model_revision TEXT NOT NULL CHECK (length(model_revision) BETWEEN 1 AND 128),
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    error_code TEXT CHECK (error_code IN (
        'invalid_configuration',
        'runtime_unavailable',
        'model_not_installed',
        'invalid_request',
        'audio_unavailable',
        'audio_outside_root',
        'model_revision_mismatch',
        'queue_full',
        'worker_unavailable',
        'circuit_open',
        'startup_timeout',
        'handshake_rejected',
        'worker_crashed',
        'job_timeout',
        'job_cancelled',
        'invalid_response',
        'cancellation_failed',
        'supervisor_stopping',
        'supervisor_stopped',
        'shutdown_timeout',
        'shutdown_failed'
    )),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    latest_result_revision INTEGER CHECK (latest_result_revision > 0),
    latest_result_revision_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (job_id, meeting_id),
    CHECK (
        (status IN ('queued', 'running', 'succeeded') AND error_code IS NULL)
        OR (status = 'failed' AND error_code IS NOT NULL)
        OR (status = 'cancelled' AND error_code = 'job_cancelled')
    ),
    CHECK (
        (latest_result_revision IS NULL AND latest_result_revision_id IS NULL)
        OR (latest_result_revision IS NOT NULL AND latest_result_revision_id IS NOT NULL)
    ),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS diarization_result_revisions (
    result_revision_id TEXT PRIMARY KEY NOT NULL CHECK (length(result_revision_id) BETWEEN 1 AND 128),
    job_id TEXT NOT NULL CHECK (length(job_id) BETWEEN 1 AND 128),
    meeting_id TEXT NOT NULL CHECK (length(meeting_id) BETWEEN 1 AND 128),
    revision INTEGER NOT NULL CHECK (revision > 0),
    window_id TEXT NOT NULL CHECK (length(window_id) BETWEEN 1 AND 128),
    window_start_frame INTEGER NOT NULL CHECK (window_start_frame >= 0),
    window_end_frame INTEGER NOT NULL CHECK (window_end_frame > window_start_frame),
    model_revision TEXT NOT NULL CHECK (length(model_revision) BETWEEN 1 AND 128),
    status TEXT NOT NULL CHECK (status IN ('succeeded', 'failed', 'cancelled')),
    error_code TEXT CHECK (error_code IN (
        'invalid_configuration',
        'runtime_unavailable',
        'model_not_installed',
        'invalid_request',
        'audio_unavailable',
        'audio_outside_root',
        'model_revision_mismatch',
        'queue_full',
        'worker_unavailable',
        'circuit_open',
        'startup_timeout',
        'handshake_rejected',
        'worker_crashed',
        'job_timeout',
        'job_cancelled',
        'invalid_response',
        'cancellation_failed',
        'supervisor_stopping',
        'supervisor_stopped',
        'shutdown_timeout',
        'shutdown_failed'
    )),
    created_at TEXT NOT NULL,
    UNIQUE (job_id, revision),
    CHECK (
        (status = 'succeeded' AND error_code IS NULL)
        OR (status = 'failed' AND error_code IS NOT NULL)
        OR (status = 'cancelled' AND error_code = 'job_cancelled')
    ),
    FOREIGN KEY (job_id, meeting_id)
        REFERENCES diarization_jobs(job_id, meeting_id) ON DELETE CASCADE,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS diarization_result_segments (
    result_revision_id TEXT NOT NULL,
    segment_index INTEGER NOT NULL CHECK (segment_index >= 0),
    start_frame INTEGER NOT NULL CHECK (start_frame >= 0),
    end_frame INTEGER NOT NULL CHECK (end_frame > start_frame),
    speaker TEXT NOT NULL CHECK (length(speaker) BETWEEN 1 AND 128),
    transcript TEXT NOT NULL CHECK (length(transcript) BETWEEN 1 AND 16384),
    PRIMARY KEY (result_revision_id, segment_index),
    FOREIGN KEY (result_revision_id)
        REFERENCES diarization_result_revisions(result_revision_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_diarization_jobs_recovery
    ON diarization_jobs(meeting_id, status, created_at);

CREATE INDEX IF NOT EXISTS idx_diarization_results_job_revision
    ON diarization_result_revisions(job_id, revision DESC);

CREATE TRIGGER IF NOT EXISTS trg_diarization_result_identity
BEFORE INSERT ON diarization_result_revisions
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1
    FROM diarization_jobs AS jobs
    WHERE jobs.job_id = NEW.job_id
      AND jobs.meeting_id = NEW.meeting_id
      AND jobs.window_id = NEW.window_id
      AND jobs.window_start_frame = NEW.window_start_frame
      AND jobs.window_end_frame = NEW.window_end_frame
      AND jobs.model_revision = NEW.model_revision
)
BEGIN
    SELECT RAISE(ABORT, 'diarization result identity mismatch');
END;

CREATE TRIGGER IF NOT EXISTS trg_diarization_segment_identity
BEFORE INSERT ON diarization_result_segments
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1
    FROM diarization_result_revisions AS revisions
    WHERE revisions.result_revision_id = NEW.result_revision_id
      AND revisions.status = 'succeeded'
      AND NEW.start_frame >= revisions.window_start_frame
      AND NEW.end_frame <= revisions.window_end_frame
)
BEGIN
    SELECT RAISE(ABORT, 'diarization segment outside successful result');
END;

CREATE TRIGGER IF NOT EXISTS trg_diarization_latest_result
AFTER INSERT ON diarization_result_revisions
FOR EACH ROW
WHEN (
    SELECT latest_result_revision
    FROM diarization_jobs
    WHERE job_id = NEW.job_id
) IS NULL
OR NEW.revision > (
    SELECT latest_result_revision
    FROM diarization_jobs
    WHERE job_id = NEW.job_id
)
BEGIN
    UPDATE diarization_jobs
    SET latest_result_revision = NEW.revision,
        latest_result_revision_id = NEW.result_revision_id,
        status = NEW.status,
        error_code = NEW.error_code,
        updated_at = NEW.created_at
    WHERE job_id = NEW.job_id;
END;

CREATE TRIGGER IF NOT EXISTS trg_diarization_job_latest_identity
BEFORE UPDATE OF latest_result_revision, latest_result_revision_id ON diarization_jobs
FOR EACH ROW
WHEN NEW.latest_result_revision IS NOT NULL
AND NOT EXISTS (
    SELECT 1
    FROM diarization_result_revisions AS revisions
    WHERE revisions.result_revision_id = NEW.latest_result_revision_id
      AND revisions.job_id = NEW.job_id
      AND revisions.meeting_id = NEW.meeting_id
      AND revisions.revision = NEW.latest_result_revision
)
BEGIN
    SELECT RAISE(ABORT, 'diarization latest result identity mismatch');
END;
