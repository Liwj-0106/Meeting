-- Revision-bound bilingual text translation storage.
--
-- Every row stores a complete translated snapshot. Provider token deltas are
-- transient transport data and intentionally have no durable column here.

CREATE TABLE IF NOT EXISTS translation_glossaries (
    glossary_id TEXT PRIMARY KEY NOT NULL
        CHECK (length(glossary_id) BETWEEN 1 AND 256),
    meeting_id TEXT,
    name TEXT NOT NULL
        CHECK (length(trim(name)) BETWEEN 1 AND 128),
    source_language TEXT COLLATE NOCASE NOT NULL
        CHECK (
            length(source_language) BETWEEN 1 AND 35
            AND source_language NOT GLOB '*[^A-Za-z0-9-]*'
        ),
    target_language TEXT COLLATE NOCASE NOT NULL
        CHECK (
            length(target_language) BETWEEN 1 AND 35
            AND target_language NOT GLOB '*[^A-Za-z0-9-]*'
        ),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    updated_at TEXT NOT NULL CHECK (length(updated_at) > 0),
    CHECK (lower(source_language) <> lower(target_language)),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_translation_glossaries_meeting_languages
    ON translation_glossaries(meeting_id, source_language, target_language, archived);

CREATE TABLE IF NOT EXISTS translation_glossary_versions (
    glossary_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    content_hash TEXT NOT NULL
        CHECK (
            length(content_hash) = 64
            AND content_hash NOT GLOB '*[^0-9a-f]*'
        ),
    -- Normalized [{"source":"...","target":"..."}] snapshot. Versions
    -- are immutable, so an edit inserts a new complete array.
    entries_json TEXT NOT NULL
        CHECK (
            length(entries_json) BETWEEN 2 AND 1048576
            AND json_valid(entries_json)
            AND json_type(entries_json) = 'array'
        ),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    PRIMARY KEY (glossary_id, version),
    UNIQUE (glossary_id, content_hash),
    UNIQUE (glossary_id, version, content_hash),
    FOREIGN KEY (glossary_id) REFERENCES translation_glossaries(glossary_id)
        ON DELETE CASCADE
);

CREATE TRIGGER IF NOT EXISTS trg_translation_glossary_versions_immutable
BEFORE UPDATE ON translation_glossary_versions
BEGIN
    SELECT RAISE(ABORT, 'translation glossary versions are immutable');
END;

-- This exact index permits a composite foreign key that proves the source
-- event, meeting, utterance and source revision all describe the same row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_utterance_revisions_translation_binding
    ON utterance_revisions(event_id, meeting_id, utterance_id, revision, event_kind);

CREATE TABLE IF NOT EXISTS translation_revisions (
    translation_event_id TEXT PRIMARY KEY NOT NULL
        CHECK (length(translation_event_id) BETWEEN 1 AND 256),
    meeting_id TEXT NOT NULL,
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
    source_text_hash TEXT NOT NULL
        CHECK (
            length(source_text_hash) = 64
            AND source_text_hash NOT GLOB '*[^0-9a-f]*'
        ),
    translation_revision INTEGER NOT NULL CHECK (translation_revision >= 1),
    generation INTEGER NOT NULL CHECK (generation >= 1),
    event_kind TEXT NOT NULL
        CHECK (event_kind IN ('snapshot', 'retraction', 'error')),
    status TEXT NOT NULL
        CHECK (status IN ('partial', 'final', 'reused', 'retracted', 'failed')),
    source_language TEXT COLLATE NOCASE NOT NULL
        CHECK (
            length(source_language) BETWEEN 1 AND 35
            AND source_language NOT GLOB '*[^A-Za-z0-9-]*'
        ),
    target_language TEXT COLLATE NOCASE NOT NULL
        CHECK (
            length(target_language) BETWEEN 1 AND 35
            AND target_language NOT GLOB '*[^A-Za-z0-9-]*'
        ),
    translated_text TEXT,
    provider TEXT CHECK (provider IS NULL OR length(provider) BETWEEN 1 AND 128),
    model TEXT CHECK (model IS NULL OR length(model) BETWEEN 1 AND 256),
    speaker_id TEXT CHECK (speaker_id IS NULL OR length(speaker_id) BETWEEN 1 AND 256),
    glossary_id TEXT,
    glossary_version INTEGER,
    glossary_content_hash TEXT,
    reused_from_event_id TEXT,
    latency_ms INTEGER CHECK (latency_ms IS NULL OR latency_ms >= 0),
    error_code TEXT CHECK (
        error_code IS NULL
        OR error_code IN ('provider_rejected', 'provider_failed', 'invalid_response')
    ),
    error_message TEXT CHECK (error_message IS NULL OR length(error_message) <= 512),
    retryable INTEGER CHECK (retryable IS NULL OR retryable IN (0, 1)),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    CHECK (lower(source_language) <> lower(target_language)),
    CHECK (
        (status IN ('partial', 'final', 'reused')
            AND event_kind = 'snapshot'
            AND translated_text IS NOT NULL
            AND length(trim(translated_text)) > 0
            AND length(translated_text) <= 65536
            AND instr(translated_text, char(0)) = 0
            AND provider IS NOT NULL)
        OR (status = 'retracted'
            AND event_kind = 'retraction'
            AND translated_text IS NULL
            AND provider IS NULL
            AND model IS NULL
            AND latency_ms IS NULL)
        OR (status = 'failed'
            AND event_kind = 'error'
            AND translated_text IS NULL
            AND latency_ms IS NULL)
    ),
    CHECK (
        (status = 'failed'
            AND provider IS NOT NULL
            AND error_code IS NOT NULL
            AND error_message IS NOT NULL
            AND length(trim(error_message)) > 0
            AND retryable IS NOT NULL)
        OR (status <> 'failed' AND error_code IS NULL AND error_message IS NULL AND retryable IS NULL)
    ),
    CHECK (
        (status = 'reused' AND reused_from_event_id IS NOT NULL)
        OR (status <> 'reused' AND reused_from_event_id IS NULL)
    ),
    CHECK (
        (status = 'partial' AND source_kind = 'partial')
        OR (status = 'final' AND source_kind IN (
            'final', 'correction', 'speaker_update', 'language_update'
        ))
        OR (status = 'reused' AND source_kind = 'speaker_update')
        OR (status = 'retracted' AND source_kind = 'retraction')
        OR (status = 'failed' AND source_kind <> 'retraction')
    ),
    CHECK (
        (glossary_id IS NULL AND glossary_version IS NULL AND glossary_content_hash IS NULL)
        OR (glossary_id IS NOT NULL
            AND glossary_version >= 1
            AND length(glossary_content_hash) = 64
            AND glossary_content_hash NOT GLOB '*[^0-9a-f]*')
    ),
    UNIQUE (meeting_id, utterance_id, target_language, translation_revision),
    UNIQUE (meeting_id, utterance_id, target_language, generation),
    UNIQUE (
        meeting_id,
        utterance_id,
        target_language,
        source_event_id,
        source_revision,
        source_text_hash,
        generation
    ),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE,
    FOREIGN KEY (source_event_id, meeting_id, utterance_id, source_revision, source_kind)
        REFERENCES utterance_revisions(
            event_id,
            meeting_id,
            utterance_id,
            revision,
            event_kind
        )
        ON DELETE CASCADE,
    FOREIGN KEY (glossary_id, glossary_version, glossary_content_hash)
        REFERENCES translation_glossary_versions(glossary_id, version, content_hash),
    FOREIGN KEY (reused_from_event_id, meeting_id, utterance_id, target_language)
        REFERENCES translation_revisions(
            translation_event_id,
            meeting_id,
            utterance_id,
            target_language
        )
);

CREATE INDEX IF NOT EXISTS idx_translation_revisions_source_binding
    ON translation_revisions(
        meeting_id,
        utterance_id,
        source_revision,
        source_event_id,
        source_text_hash
    );

CREATE INDEX IF NOT EXISTS idx_translation_revisions_target_order
    ON translation_revisions(
        meeting_id,
        target_language,
        translation_revision,
        utterance_id
    );

CREATE INDEX IF NOT EXISTS idx_translation_revisions_generation
    ON translation_revisions(meeting_id, utterance_id, target_language, generation);

CREATE INDEX IF NOT EXISTS idx_translation_revisions_glossary
    ON translation_revisions(glossary_id, glossary_version)
    WHERE glossary_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_translation_revisions_reuse_scope
    ON translation_revisions(
        translation_event_id,
        meeting_id,
        utterance_id,
        target_language
    );

-- A reuse edge is not merely a scoped event reference. It must point to a
-- completed snapshot produced from the same translation inputs and source
-- text. This trigger also protects the invariant when foreign keys are
-- accidentally disabled by a standalone SQLite client.
CREATE TRIGGER IF NOT EXISTS trg_translation_revisions_validate_reuse
BEFORE INSERT ON translation_revisions
WHEN NEW.status = 'reused'
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1
        FROM translation_revisions AS previous
        WHERE previous.translation_event_id = NEW.reused_from_event_id
          AND previous.meeting_id = NEW.meeting_id
          AND previous.utterance_id = NEW.utterance_id
          AND previous.target_language = NEW.target_language
          AND previous.status IN ('final', 'reused')
          AND previous.source_text_hash = NEW.source_text_hash
          AND previous.source_language = NEW.source_language
          AND previous.translated_text = NEW.translated_text
          AND previous.provider = NEW.provider
          AND previous.model IS NEW.model
          AND previous.glossary_id IS NEW.glossary_id
          AND previous.glossary_version IS NEW.glossary_version
          AND previous.glossary_content_hash IS NEW.glossary_content_hash
    ) THEN RAISE(ABORT, 'reused translation must reference a completed same-scope snapshot') END;
END;

CREATE TRIGGER IF NOT EXISTS trg_translation_revisions_immutable
BEFORE UPDATE ON translation_revisions
BEGIN
    SELECT RAISE(ABORT, 'translation revisions are immutable');
END;

-- Materialized current view for the caption renderer. It repeats the source
-- triple so a consumer can validate the projection without joining history.
CREATE TABLE IF NOT EXISTS translation_latest (
    meeting_id TEXT NOT NULL,
    utterance_id TEXT NOT NULL,
    target_language TEXT COLLATE NOCASE NOT NULL
        CHECK (
            length(target_language) BETWEEN 1 AND 35
            AND target_language NOT GLOB '*[^A-Za-z0-9-]*'
        ),
    translation_event_id TEXT NOT NULL UNIQUE,
    source_event_id TEXT NOT NULL,
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
    source_text_hash TEXT NOT NULL
        CHECK (
            length(source_text_hash) = 64
            AND source_text_hash NOT GLOB '*[^0-9a-f]*'
        ),
    translation_revision INTEGER NOT NULL CHECK (translation_revision >= 1),
    generation INTEGER NOT NULL CHECK (generation >= 1),
    status TEXT NOT NULL
        CHECK (status IN ('partial', 'final', 'reused', 'retracted', 'failed')),
    source_language TEXT COLLATE NOCASE NOT NULL
        CHECK (
            length(source_language) BETWEEN 1 AND 35
            AND source_language NOT GLOB '*[^A-Za-z0-9-]*'
        ),
    translated_text TEXT,
    provider TEXT,
    model TEXT,
    speaker_id TEXT,
    glossary_id TEXT,
    glossary_version INTEGER,
    glossary_content_hash TEXT,
    reused_from_event_id TEXT,
    latency_ms INTEGER CHECK (latency_ms IS NULL OR latency_ms >= 0),
    error_code TEXT CHECK (
        error_code IS NULL
        OR error_code IN ('provider_rejected', 'provider_failed', 'invalid_response')
    ),
    error_message TEXT CHECK (error_message IS NULL OR length(error_message) <= 512),
    retryable INTEGER CHECK (retryable IS NULL OR retryable IN (0, 1)),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    CHECK (lower(source_language) <> lower(target_language)),
    CHECK (
        (status IN ('partial', 'final', 'reused')
            AND translated_text IS NOT NULL
            AND length(trim(translated_text)) > 0
            AND length(translated_text) <= 65536
            AND instr(translated_text, char(0)) = 0
            AND provider IS NOT NULL
            AND error_code IS NULL
            AND error_message IS NULL
            AND retryable IS NULL)
        OR (status = 'retracted'
            AND translated_text IS NULL
            AND provider IS NULL
            AND model IS NULL
            AND latency_ms IS NULL
            AND error_code IS NULL
            AND error_message IS NULL
            AND retryable IS NULL)
        OR (status = 'failed'
            AND translated_text IS NULL
            AND latency_ms IS NULL
            AND provider IS NOT NULL
            AND error_code IS NOT NULL
            AND error_message IS NOT NULL
            AND length(trim(error_message)) > 0
            AND retryable IS NOT NULL)
    ),
    CHECK (
        (status = 'reused' AND reused_from_event_id IS NOT NULL)
        OR (status <> 'reused' AND reused_from_event_id IS NULL)
    ),
    CHECK (
        (status = 'partial' AND source_kind = 'partial')
        OR (status = 'final' AND source_kind IN (
            'final', 'correction', 'speaker_update', 'language_update'
        ))
        OR (status = 'reused' AND source_kind = 'speaker_update')
        OR (status = 'retracted' AND source_kind = 'retraction')
        OR (status = 'failed' AND source_kind <> 'retraction')
    ),
    CHECK (
        (glossary_id IS NULL AND glossary_version IS NULL AND glossary_content_hash IS NULL)
        OR (glossary_id IS NOT NULL
            AND glossary_version >= 1
            AND length(glossary_content_hash) = 64
            AND glossary_content_hash NOT GLOB '*[^0-9a-f]*')
    ),
    PRIMARY KEY (meeting_id, utterance_id, target_language),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE,
    FOREIGN KEY (translation_event_id, meeting_id, utterance_id, target_language)
        REFERENCES translation_revisions(
            translation_event_id,
            meeting_id,
            utterance_id,
            target_language
        ) ON DELETE CASCADE,
    FOREIGN KEY (glossary_id, glossary_version, glossary_content_hash)
        REFERENCES translation_glossary_versions(glossary_id, version, content_hash)
);

CREATE INDEX IF NOT EXISTS idx_translation_latest_meeting_target
    ON translation_latest(meeting_id, target_language, source_revision, utterance_id);

CREATE TRIGGER IF NOT EXISTS trg_translation_revisions_project_latest
AFTER INSERT ON translation_revisions
BEGIN
    INSERT INTO translation_latest (
        meeting_id,
        utterance_id,
        target_language,
        translation_event_id,
        source_event_id,
        source_revision,
        source_kind,
        source_text_hash,
        translation_revision,
        generation,
        status,
        source_language,
        translated_text,
        provider,
        model,
        speaker_id,
        glossary_id,
        glossary_version,
        glossary_content_hash,
        reused_from_event_id,
        latency_ms,
        error_code,
        error_message,
        retryable,
        created_at
    ) VALUES (
        NEW.meeting_id,
        NEW.utterance_id,
        NEW.target_language,
        NEW.translation_event_id,
        NEW.source_event_id,
        NEW.source_revision,
        NEW.source_kind,
        NEW.source_text_hash,
        NEW.translation_revision,
        NEW.generation,
        NEW.status,
        NEW.source_language,
        NEW.translated_text,
        NEW.provider,
        NEW.model,
        NEW.speaker_id,
        NEW.glossary_id,
        NEW.glossary_version,
        NEW.glossary_content_hash,
        NEW.reused_from_event_id,
        NEW.latency_ms,
        NEW.error_code,
        NEW.error_message,
        NEW.retryable,
        NEW.created_at
    )
    ON CONFLICT(meeting_id, utterance_id, target_language) DO UPDATE SET
        translation_event_id = excluded.translation_event_id,
        source_event_id = excluded.source_event_id,
        source_revision = excluded.source_revision,
        source_kind = excluded.source_kind,
        source_text_hash = excluded.source_text_hash,
        translation_revision = excluded.translation_revision,
        generation = excluded.generation,
        status = excluded.status,
        source_language = excluded.source_language,
        translated_text = excluded.translated_text,
        provider = excluded.provider,
        model = excluded.model,
        speaker_id = excluded.speaker_id,
        glossary_id = excluded.glossary_id,
        glossary_version = excluded.glossary_version,
        glossary_content_hash = excluded.glossary_content_hash,
        reused_from_event_id = excluded.reused_from_event_id,
        latency_ms = excluded.latency_ms,
        error_code = excluded.error_code,
        error_message = excluded.error_message,
        retryable = excluded.retryable,
        created_at = excluded.created_at
    WHERE
        excluded.source_revision > translation_latest.source_revision
        OR (
            excluded.source_revision = translation_latest.source_revision
            AND CASE excluded.source_kind
                WHEN 'partial' THEN 1
                WHEN 'final' THEN 2
                WHEN 'correction' THEN 3
                WHEN 'speaker_update' THEN 4
                WHEN 'language_update' THEN 4
                WHEN 'retraction' THEN 5
            END > CASE translation_latest.source_kind
                WHEN 'partial' THEN 1
                WHEN 'final' THEN 2
                WHEN 'correction' THEN 3
                WHEN 'speaker_update' THEN 4
                WHEN 'language_update' THEN 4
                WHEN 'retraction' THEN 5
            END
        )
        OR (
            excluded.source_revision = translation_latest.source_revision
            AND CASE excluded.source_kind
                WHEN 'partial' THEN 1
                WHEN 'final' THEN 2
                WHEN 'correction' THEN 3
                WHEN 'speaker_update' THEN 4
                WHEN 'language_update' THEN 4
                WHEN 'retraction' THEN 5
            END = CASE translation_latest.source_kind
                WHEN 'partial' THEN 1
                WHEN 'final' THEN 2
                WHEN 'correction' THEN 3
                WHEN 'speaker_update' THEN 4
                WHEN 'language_update' THEN 4
                WHEN 'retraction' THEN 5
            END
            AND excluded.source_event_id = translation_latest.source_event_id
            AND excluded.source_text_hash = translation_latest.source_text_hash
            AND (
                excluded.generation > translation_latest.generation
                OR (
                    excluded.generation = translation_latest.generation
                    AND excluded.translation_revision > translation_latest.translation_revision
                )
            )
        );
END;
