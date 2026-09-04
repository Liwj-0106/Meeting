-- Runtime configuration for opt-in online live text translation.
-- API keys remain Rust-only: public commands project only key presence.

CREATE TABLE IF NOT EXISTS live_translation_settings (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    source_language TEXT COLLATE NOCASE NOT NULL DEFAULT 'auto'
        CHECK (
            length(source_language) BETWEEN 1 AND 35
            AND source_language NOT GLOB '*[^A-Za-z0-9-]*'
        ),
    target_language TEXT COLLATE NOCASE NOT NULL DEFAULT 'zh-CN'
        CHECK (
            length(target_language) BETWEEN 1 AND 35
            AND target_language NOT GLOB '*[^A-Za-z0-9-]*'
        ),
    provider TEXT NOT NULL DEFAULT 'openai'
        CHECK (provider IN ('openai', 'openai_compatible')),
    model TEXT NOT NULL DEFAULT 'gpt-4o-mini'
        CHECK (length(model) BETWEEN 1 AND 256),
    endpoint TEXT NOT NULL DEFAULT 'https://api.openai.com/v1'
        CHECK (length(endpoint) BETWEEN 1 AND 2048),
    api_key TEXT CHECK (api_key IS NULL OR length(api_key) BETWEEN 1 AND 8192),
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        CHECK (length(updated_at) > 0),
    CHECK (lower(source_language) <> lower(target_language))
);

INSERT OR IGNORE INTO live_translation_settings (id) VALUES (1);

