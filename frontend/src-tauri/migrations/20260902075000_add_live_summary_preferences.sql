-- Global defaults for future live-summary sessions.
-- API keys stay in the existing credential-bearing settings row. Prompt text
-- is write-only over IPC and is never copied into summary revisions.
CREATE TABLE IF NOT EXISTS live_summary_preferences (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    template_id TEXT NOT NULL DEFAULT 'standard_meeting'
        CHECK (
            length(template_id) BETWEEN 1 AND 64
            AND template_id NOT GLOB '*[^A-Za-z0-9_-]*'
        ),
    custom_prompt TEXT CHECK (
        custom_prompt IS NULL
        OR (
            length(trim(custom_prompt)) BETWEEN 1 AND 8000
            AND instr(custom_prompt, char(0)) = 0
        )
    ),
    updated_at TEXT NOT NULL CHECK (length(updated_at) > 0)
);

INSERT OR IGNORE INTO live_summary_preferences (
    id,
    template_id,
    custom_prompt,
    updated_at
) VALUES (
    1,
    'standard_meeting',
    NULL,
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
);
