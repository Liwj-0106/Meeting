-- Durable, session-scoped staging for complete live-translation snapshots.
--
-- A live ASR session does not have a canonical meeting row until the recording
-- is saved. Keeping these Rust-owned snapshots in a separate table lets an
-- overlay reload or process restart recover them without inventing a meeting
-- ID or weakening the foreign keys on `translation_revisions`.

CREATE TABLE IF NOT EXISTS live_translation_staging (
    translation_event_id TEXT PRIMARY KEY NOT NULL
        CHECK (length(translation_event_id) BETWEEN 1 AND 256),
    session_id TEXT NOT NULL CHECK (length(session_id) BETWEEN 1 AND 256),
    utterance_id TEXT NOT NULL CHECK (length(utterance_id) BETWEEN 1 AND 256),
    source_event_id TEXT NOT NULL CHECK (length(source_event_id) BETWEEN 1 AND 256),
    source_revision INTEGER NOT NULL CHECK (source_revision >= 0),
    source_kind TEXT NOT NULL CHECK (
        source_kind IN (
            'partial',
            'final',
            'correction',
            'speaker_update',
            'language_update',
            'retraction'
        )
    ),
    source_text_hash TEXT NOT NULL CHECK (
        length(source_text_hash) = 64
        AND source_text_hash NOT GLOB '*[^0-9a-f]*'
    ),
    target_language TEXT COLLATE NOCASE NOT NULL CHECK (
        length(target_language) BETWEEN 1 AND 35
        AND target_language NOT GLOB '*[^A-Za-z0-9-]*'
    ),
    generation INTEGER NOT NULL CHECK (generation >= 1),
    translation_revision INTEGER NOT NULL CHECK (translation_revision >= 1),
    event_json TEXT NOT NULL CHECK (
        length(event_json) BETWEEN 2 AND 524288
        AND json_valid(event_json)
        AND json_type(event_json) IS 'object'
    ),
    staged_at_ms INTEGER NOT NULL CHECK (staged_at_ms >= 0),
    CHECK (
        json_type(event_json, '$.schema_version') IS 'integer'
        AND json_extract(event_json, '$.schema_version') = 1
    ),
    CHECK (
        json_type(event_json, '$.translation_event_id') IS 'text'
        AND json_extract(event_json, '$.translation_event_id') = translation_event_id
    ),
    CHECK (
        json_type(event_json, '$.source.meeting_id') IS 'text'
        AND json_extract(event_json, '$.source.meeting_id') = session_id
    ),
    CHECK (
        json_type(event_json, '$.source.utterance_id') IS 'text'
        AND json_extract(event_json, '$.source.utterance_id') = utterance_id
    ),
    CHECK (
        json_type(event_json, '$.source.event_id') IS 'text'
        AND json_extract(event_json, '$.source.event_id') = source_event_id
    ),
    CHECK (
        json_type(event_json, '$.source.revision') IS 'integer'
        AND json_extract(event_json, '$.source.revision') = source_revision
    ),
    CHECK (
        json_type(event_json, '$.source.text_hash') IS 'text'
        AND json_extract(event_json, '$.source.text_hash') = source_text_hash
    ),
    CHECK (
        json_type(event_json, '$.source_kind') IS 'text'
        AND json_extract(event_json, '$.source_kind') = source_kind
    ),
    CHECK (
        json_type(event_json, '$.target_language') IS 'text'
        AND json_extract(event_json, '$.target_language') = target_language
    ),
    CHECK (
        json_type(event_json, '$.request_fingerprint.target_language') IS 'text'
        AND json_extract(event_json, '$.request_fingerprint.target_language') = target_language
    ),
    CHECK (
        json_type(event_json, '$.generation') IS 'integer'
        AND json_extract(event_json, '$.generation') = generation
    ),
    CHECK (
        json_type(event_json, '$.translation_revision') IS 'integer'
        AND json_extract(event_json, '$.translation_revision') = translation_revision
    ),
    UNIQUE (session_id, utterance_id, target_language, generation),
    UNIQUE (session_id, utterance_id, target_language, translation_revision)
);

CREATE INDEX IF NOT EXISTS idx_live_translation_staging_session
    ON live_translation_staging(session_id, staged_at_ms, translation_event_id);

CREATE INDEX IF NOT EXISTS idx_live_translation_staging_exact_source
    ON live_translation_staging(
        session_id,
        utterance_id,
        source_event_id,
        source_revision,
        source_kind,
        source_text_hash,
        target_language,
        generation,
        translation_revision
    );

CREATE TRIGGER IF NOT EXISTS trg_live_translation_staging_immutable
BEFORE UPDATE ON live_translation_staging
BEGIN
    SELECT RAISE(ABORT, 'staged translation events are immutable');
END;
