-- Preserve every immutable provider event, including a partial and final that
-- legitimately share the same provider revision. The original Stage 1
-- migration is kept unchanged so databases that already applied it retain a
-- valid sqlx migration checksum; this follow-up migration removes only the
-- obsolete (meeting_id, utterance_id, revision) uniqueness constraint.

CREATE TABLE utterance_revisions_v2 (
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
    diarization_provider TEXT,
    diarization_model TEXT,
    diarization_model_revision TEXT,
    diarization_revision INTEGER,
    diarization_window_id TEXT,
    diarization_window_start_frame INTEGER,
    diarization_window_end_frame INTEGER,
    diarization_status TEXT,
    diarization_latency_ms INTEGER,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

INSERT INTO utterance_revisions_v2 (
    event_id, meeting_id, schema_version, session_id, utterance_id, revision,
    event_kind, is_stable, transcript, timestamp,
    sequence_id, start_ms, end_ms,
    audio_start_time, audio_end_time, duration, audio_source,
    speaker_id, speaker_local_label, speaker_display_name,
    speaker_confidence, speaker_status, language,
    asr_provider, asr_model, asr_confidence, asr_latency_ms,
    replaces_event_id, provider_event_id, trace_id, created_at,
    diarization_provider, diarization_model, diarization_model_revision,
    diarization_revision, diarization_window_id,
    diarization_window_start_frame, diarization_window_end_frame,
    diarization_status, diarization_latency_ms
)
SELECT
    event_id, meeting_id, schema_version, session_id, utterance_id, revision,
    event_kind, is_stable, transcript, timestamp,
    sequence_id, start_ms, end_ms,
    audio_start_time, audio_end_time, duration, audio_source,
    speaker_id, speaker_local_label, speaker_display_name,
    speaker_confidence, speaker_status, language,
    asr_provider, asr_model, asr_confidence, asr_latency_ms,
    replaces_event_id, provider_event_id, trace_id, created_at,
    diarization_provider, diarization_model, diarization_model_revision,
    diarization_revision, diarization_window_id,
    diarization_window_start_frame, diarization_window_end_frame,
    diarization_status, diarization_latency_ms
FROM utterance_revisions;

DROP TABLE utterance_revisions;
ALTER TABLE utterance_revisions_v2 RENAME TO utterance_revisions;

CREATE INDEX idx_utterance_revisions_meeting_time
    ON utterance_revisions(meeting_id, audio_start_time, utterance_id, revision);

CREATE INDEX idx_utterance_revisions_meeting_start_ms
    ON utterance_revisions(meeting_id, start_ms, utterance_id, revision);

CREATE INDEX idx_utterance_revisions_utterance
    ON utterance_revisions(
        meeting_id,
        utterance_id,
        revision,
        is_stable,
        event_kind,
        created_at,
        event_id
    );

CREATE INDEX idx_utterance_revisions_provider_event
    ON utterance_revisions(provider_event_id)
    WHERE provider_event_id IS NOT NULL;

CREATE INDEX idx_utterance_revisions_diarization_window
    ON utterance_revisions(meeting_id, diarization_window_id, diarization_revision)
    WHERE diarization_window_id IS NOT NULL;
