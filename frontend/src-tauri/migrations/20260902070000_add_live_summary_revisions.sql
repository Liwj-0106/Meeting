-- Versioned, evidence-bound live summary storage.
--
-- Provider credentials, prompt text and raw provider responses are deliberately
-- absent. A row in live_summary_revisions represents one complete, validated
-- summary snapshot; items and evidence are inserted in the same transaction.

-- This exact index lets summary evidence prove that its event, meeting,
-- utterance and transcript revision all refer to the same immutable row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_utterance_revisions_summary_binding
    ON utterance_revisions(event_id, meeting_id, utterance_id, revision);

CREATE TABLE IF NOT EXISTS live_summary_revisions (
    summary_revision_id TEXT PRIMARY KEY NOT NULL
        CHECK (length(summary_revision_id) BETWEEN 1 AND 256),
    meeting_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    generation INTEGER NOT NULL CHECK (generation >= 1),
    transcript_cursor INTEGER NOT NULL CHECK (transcript_cursor >= 0),
    snapshot_hash TEXT NOT NULL
        CHECK (
            length(snapshot_hash) = 64
            AND snapshot_hash NOT GLOB '*[^0-9a-f]*'
        ),
    revision_type TEXT NOT NULL CHECK (revision_type IN ('live', 'final')),
    provider TEXT NOT NULL CHECK (length(trim(provider)) BETWEEN 1 AND 128),
    model TEXT CHECK (model IS NULL OR length(model) BETWEEN 1 AND 256),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    UNIQUE (meeting_id, revision),
    UNIQUE (meeting_id, generation),
    UNIQUE (summary_revision_id, meeting_id),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_live_summary_revisions_meeting_generation
    ON live_summary_revisions(meeting_id, generation, revision);

CREATE TABLE IF NOT EXISTS live_summary_sessions (
    meeting_id TEXT PRIMARY KEY NOT NULL,
    latest_revision_id TEXT,
    latest_revision INTEGER NOT NULL DEFAULT 0 CHECK (latest_revision >= 0),
    latest_generation INTEGER NOT NULL DEFAULT 0 CHECK (latest_generation >= 0),
    transcript_cursor INTEGER NOT NULL DEFAULT 0 CHECK (transcript_cursor >= 0),
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'finalized')),
    updated_at TEXT NOT NULL CHECK (length(updated_at) > 0),
    CHECK (
        (latest_revision_id IS NULL AND latest_revision = 0 AND latest_generation = 0)
        OR (latest_revision_id IS NOT NULL AND latest_revision >= 1 AND latest_generation >= 1)
    ),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE,
    FOREIGN KEY (latest_revision_id) REFERENCES live_summary_revisions(summary_revision_id)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS live_summary_items (
    summary_revision_id TEXT NOT NULL,
    item_id TEXT NOT NULL CHECK (length(item_id) BETWEEN 1 AND 256),
    position INTEGER NOT NULL CHECK (position >= 0),
    kind TEXT NOT NULL
        CHECK (kind IN ('topic', 'decision', 'action_item', 'risk', 'open_question')),
    title TEXT NOT NULL
        CHECK (
            length(trim(title)) BETWEEN 1 AND 512
            AND instr(title, char(0)) = 0
        ),
    body TEXT NOT NULL
        CHECK (
            length(trim(body)) BETWEEN 1 AND 65536
            AND instr(body, char(0)) = 0
        ),
    owner TEXT CHECK (
        owner IS NULL
        OR (length(trim(owner)) BETWEEN 1 AND 256 AND instr(owner, char(0)) = 0)
    ),
    due_at TEXT CHECK (
        due_at IS NULL
        OR (length(trim(due_at)) BETWEEN 1 AND 128 AND instr(due_at, char(0)) = 0)
    ),
    status TEXT NOT NULL CHECK (status IN ('active', 'needs_review', 'retracted')),
    PRIMARY KEY (summary_revision_id, item_id),
    UNIQUE (summary_revision_id, position),
    FOREIGN KEY (summary_revision_id)
        REFERENCES live_summary_revisions(summary_revision_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_live_summary_items_revision_position
    ON live_summary_items(summary_revision_id, position);

CREATE TABLE IF NOT EXISTS live_summary_evidence (
    summary_revision_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    evidence_index INTEGER NOT NULL CHECK (evidence_index >= 0),
    meeting_id TEXT NOT NULL,
    utterance_id TEXT NOT NULL CHECK (length(utterance_id) BETWEEN 1 AND 256),
    source_revision INTEGER NOT NULL CHECK (source_revision >= 0),
    source_event_id TEXT NOT NULL CHECK (length(source_event_id) BETWEEN 1 AND 256),
    source_text_hash TEXT NOT NULL
        CHECK (
            length(source_text_hash) = 64
            AND source_text_hash NOT GLOB '*[^0-9a-f]*'
        ),
    PRIMARY KEY (summary_revision_id, item_id, evidence_index),
    UNIQUE (
        summary_revision_id,
        item_id,
        meeting_id,
        utterance_id,
        source_revision,
        source_event_id,
        source_text_hash
    ),
    FOREIGN KEY (summary_revision_id, item_id)
        REFERENCES live_summary_items(summary_revision_id, item_id) ON DELETE CASCADE,
    FOREIGN KEY (source_event_id, meeting_id, utterance_id, source_revision)
        REFERENCES utterance_revisions(event_id, meeting_id, utterance_id, revision)
        ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_live_summary_evidence_source
    ON live_summary_evidence(
        meeting_id,
        utterance_id,
        source_revision,
        source_event_id,
        source_text_hash
    );

CREATE TRIGGER IF NOT EXISTS trg_live_summary_revisions_immutable
BEFORE UPDATE ON live_summary_revisions
BEGIN
    SELECT RAISE(ABORT, 'live summary revisions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS trg_live_summary_items_immutable
BEFORE UPDATE ON live_summary_items
BEGIN
    SELECT RAISE(ABORT, 'live summary items are immutable');
END;

CREATE TRIGGER IF NOT EXISTS trg_live_summary_evidence_immutable
BEFORE UPDATE ON live_summary_evidence
BEGIN
    SELECT RAISE(ABORT, 'live summary evidence is immutable');
END;

CREATE TRIGGER IF NOT EXISTS trg_live_summary_sessions_validate_latest_insert
BEFORE INSERT ON live_summary_sessions
WHEN NEW.latest_revision_id IS NOT NULL
     AND NOT EXISTS (
         SELECT 1
         FROM live_summary_revisions
         WHERE summary_revision_id = NEW.latest_revision_id
           AND meeting_id = NEW.meeting_id
           AND revision = NEW.latest_revision
           AND generation = NEW.latest_generation
     )
BEGIN
    SELECT RAISE(ABORT, 'live summary latest projection does not match its meeting');
END;

CREATE TRIGGER IF NOT EXISTS trg_live_summary_sessions_validate_latest_update
BEFORE UPDATE OF latest_revision_id, latest_revision, latest_generation
    ON live_summary_sessions
WHEN NEW.latest_revision_id IS NOT NULL
     AND NOT EXISTS (
         SELECT 1
         FROM live_summary_revisions
         WHERE summary_revision_id = NEW.latest_revision_id
           AND meeting_id = NEW.meeting_id
           AND revision = NEW.latest_revision
           AND generation = NEW.latest_generation
     )
BEGIN
    SELECT RAISE(ABORT, 'live summary latest projection does not match its meeting');
END;

-- Project only a strictly newer generation (or a higher revision for the same
-- generation). Delayed persistence can remain in immutable history but cannot
-- move the materialized latest pointer backwards.
CREATE TRIGGER IF NOT EXISTS trg_live_summary_revisions_project_latest
AFTER INSERT ON live_summary_revisions
BEGIN
    INSERT INTO live_summary_sessions (
        meeting_id,
        latest_revision_id,
        latest_revision,
        latest_generation,
        transcript_cursor,
        state,
        updated_at
    ) VALUES (
        NEW.meeting_id,
        NEW.summary_revision_id,
        NEW.revision,
        NEW.generation,
        NEW.transcript_cursor,
        CASE WHEN NEW.revision_type = 'final' THEN 'finalized' ELSE 'active' END,
        NEW.created_at
    )
    ON CONFLICT(meeting_id) DO UPDATE SET
        latest_revision_id = excluded.latest_revision_id,
        latest_revision = excluded.latest_revision,
        latest_generation = excluded.latest_generation,
        transcript_cursor = excluded.transcript_cursor,
        state = excluded.state,
        updated_at = excluded.updated_at
    WHERE (
            excluded.latest_generation > live_summary_sessions.latest_generation
            OR (
                excluded.latest_generation = live_summary_sessions.latest_generation
                AND excluded.latest_revision > live_summary_sessions.latest_revision
            )
          )
      AND (
            live_summary_sessions.state <> 'finalized'
            OR excluded.state = 'finalized'
          );
END;
