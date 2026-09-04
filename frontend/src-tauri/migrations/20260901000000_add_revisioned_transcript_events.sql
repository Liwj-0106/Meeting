-- Revisioned transcript event storage.
--
-- `transcripts` remains the backwards-compatible materialized view consumed by
-- the existing meeting APIs. `utterance_revisions` is the append-only history
-- used by streaming ASR, replay, diarization correction, and incremental
-- summaries.

-- Stable identity and latest revision metadata.
ALTER TABLE transcripts ADD COLUMN utterance_id TEXT;
ALTER TABLE transcripts ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
ALTER TABLE transcripts ADD COLUMN latest_event_id TEXT;
ALTER TABLE transcripts ADD COLUMN schema_version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE transcripts ADD COLUMN session_id TEXT;
ALTER TABLE transcripts ADD COLUMN event_kind TEXT NOT NULL DEFAULT 'final';
ALTER TABLE transcripts ADD COLUMN is_stable INTEGER NOT NULL DEFAULT 1;
ALTER TABLE transcripts ADD COLUMN sequence_id INTEGER;
ALTER TABLE transcripts ADD COLUMN start_ms INTEGER;
ALTER TABLE transcripts ADD COLUMN end_ms INTEGER;

-- The legacy `speaker` column actually stored the capture source (`mic` or
-- `system`). Keep it for old clients, but expose its meaning explicitly and use
-- `speaker_id` only for a diarized human speaker identity.
ALTER TABLE transcripts ADD COLUMN audio_source TEXT;
ALTER TABLE transcripts ADD COLUMN speaker_id TEXT;
ALTER TABLE transcripts ADD COLUMN speaker_local_label TEXT;
ALTER TABLE transcripts ADD COLUMN speaker_display_name TEXT;
ALTER TABLE transcripts ADD COLUMN speaker_confidence REAL;
ALTER TABLE transcripts ADD COLUMN speaker_status TEXT;

-- Recognition/provenance metadata needed for provider comparison and replay.
ALTER TABLE transcripts ADD COLUMN language TEXT;
ALTER TABLE transcripts ADD COLUMN asr_provider TEXT;
ALTER TABLE transcripts ADD COLUMN asr_model TEXT;
ALTER TABLE transcripts ADD COLUMN asr_confidence REAL;
ALTER TABLE transcripts ADD COLUMN asr_latency_ms INTEGER;
ALTER TABLE transcripts ADD COLUMN replaces_event_id TEXT;
ALTER TABLE transcripts ADD COLUMN provider_event_id TEXT;
ALTER TABLE transcripts ADD COLUMN trace_id TEXT;
ALTER TABLE transcripts ADD COLUMN event_created_at TEXT;
ALTER TABLE transcripts ADD COLUMN event_updated_at TEXT;

-- Every pre-existing transcript row becomes revision 1 of its own utterance.
UPDATE transcripts
SET utterance_id = id
WHERE utterance_id IS NULL OR utterance_id = '';

UPDATE transcripts
SET latest_event_id = 'legacy-event-' || id
WHERE latest_event_id IS NULL OR latest_event_id = '';

UPDATE transcripts
SET audio_source = speaker
WHERE audio_source IS NULL AND speaker IS NOT NULL;

UPDATE transcripts
SET start_ms = CAST(ROUND(audio_start_time * 1000.0) AS INTEGER)
WHERE start_ms IS NULL AND audio_start_time IS NOT NULL;

UPDATE transcripts
SET end_ms = CAST(ROUND(audio_end_time * 1000.0) AS INTEGER)
WHERE end_ms IS NULL AND audio_end_time IS NOT NULL;

UPDATE transcripts
SET event_created_at = CURRENT_TIMESTAMP,
    event_updated_at = CURRENT_TIMESTAMP
WHERE event_created_at IS NULL OR event_updated_at IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_transcripts_meeting_utterance
    ON transcripts(meeting_id, utterance_id);

CREATE INDEX IF NOT EXISTS idx_transcripts_meeting_audio_start
    ON transcripts(meeting_id, audio_start_time);

CREATE INDEX IF NOT EXISTS idx_transcripts_meeting_speaker
    ON transcripts(meeting_id, speaker_id);

CREATE TABLE IF NOT EXISTS utterance_revisions (
    event_id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL,
    schema_version INTEGER NOT NULL DEFAULT 0 CHECK (schema_version >= 0),
    session_id TEXT,
    utterance_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    event_kind TEXT NOT NULL,
    is_stable INTEGER NOT NULL CHECK (is_stable IN (0, 1)),
    transcript TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    sequence_id INTEGER,
    start_ms INTEGER,
    end_ms INTEGER,
    audio_start_time REAL,
    audio_end_time REAL,
    duration REAL,
    audio_source TEXT,
    speaker_id TEXT,
    speaker_local_label TEXT,
    speaker_display_name TEXT,
    speaker_confidence REAL,
    speaker_status TEXT,
    language TEXT,
    asr_provider TEXT,
    asr_model TEXT,
    asr_confidence REAL,
    asr_latency_ms INTEGER,
    replaces_event_id TEXT,
    provider_event_id TEXT,
    trace_id TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(meeting_id, utterance_id, revision),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_utterance_revisions_meeting_time
    ON utterance_revisions(meeting_id, audio_start_time, utterance_id, revision);

CREATE INDEX IF NOT EXISTS idx_utterance_revisions_meeting_start_ms
    ON utterance_revisions(meeting_id, start_ms, utterance_id, revision);

CREATE INDEX IF NOT EXISTS idx_utterance_revisions_utterance
    ON utterance_revisions(meeting_id, utterance_id, revision);

CREATE INDEX IF NOT EXISTS idx_utterance_revisions_provider_event
    ON utterance_revisions(provider_event_id)
    WHERE provider_event_id IS NOT NULL;

-- Seed the revision history for databases created by previous Meetily builds.
INSERT OR IGNORE INTO utterance_revisions (
    event_id,
    meeting_id,
    schema_version,
    session_id,
    utterance_id,
    revision,
    event_kind,
    is_stable,
    transcript,
    timestamp,
    sequence_id,
    start_ms,
    end_ms,
    audio_start_time,
    audio_end_time,
    duration,
    audio_source,
    speaker_id,
    speaker_local_label,
    speaker_display_name,
    speaker_confidence,
    speaker_status,
    language,
    asr_provider,
    asr_model,
    asr_confidence,
    asr_latency_ms,
    replaces_event_id,
    provider_event_id,
    trace_id,
    created_at
)
SELECT
    latest_event_id,
    meeting_id,
    schema_version,
    session_id,
    utterance_id,
    revision,
    event_kind,
    is_stable,
    transcript,
    timestamp,
    sequence_id,
    start_ms,
    end_ms,
    audio_start_time,
    audio_end_time,
    duration,
    audio_source,
    speaker_id,
    speaker_local_label,
    speaker_display_name,
    speaker_confidence,
    speaker_status,
    language,
    asr_provider,
    asr_model,
    asr_confidence,
    asr_latency_ms,
    replaces_event_id,
    provider_event_id,
    trace_id,
    COALESCE(event_created_at, CURRENT_TIMESTAMP)
FROM transcripts
WHERE utterance_id IS NOT NULL;
