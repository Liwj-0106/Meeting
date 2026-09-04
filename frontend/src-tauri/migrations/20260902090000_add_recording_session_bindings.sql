-- Durable correlation between a Rust-created recording/ASR session and the
-- canonical meeting eventually created from it. The pending row is written at
-- recording start; assigning meeting_id happens in the same transaction that
-- creates the meeting and its transcript revisions.
--
-- The recording path itself is already stored on meetings. Only its normalized
-- SHA-256 correlation is repeated here so a renderer cannot redirect a pending
-- session to a different folder after an application restart.

CREATE TABLE recording_session_meeting_bindings (
    source_session_id TEXT PRIMARY KEY NOT NULL
        CHECK (
            length(source_session_id) BETWEEN 1 AND 256
            AND instr(source_session_id, char(0)) = 0
        ),
    recording_folder_hash TEXT NOT NULL UNIQUE
        CHECK (
            length(recording_folder_hash) = 64
            AND recording_folder_hash NOT GLOB '*[^0-9a-f]*'
        ),
    meeting_id TEXT UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('pending', 'bound')),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    updated_at TEXT NOT NULL CHECK (length(updated_at) > 0),
    bound_at TEXT,
    CHECK (
        (state = 'pending' AND meeting_id IS NULL AND bound_at IS NULL)
        OR
        (state = 'bound' AND meeting_id IS NOT NULL AND bound_at IS NOT NULL)
    ),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX idx_recording_session_meeting_bindings_state_updated
    ON recording_session_meeting_bindings(state, updated_at);
