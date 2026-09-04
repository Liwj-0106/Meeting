-- Independent diarization provenance for revisioned transcript events.
--
-- All columns are nullable so databases and events created before diarization
-- support remain valid. Recognition provenance stays in the asr_* columns.

ALTER TABLE transcripts ADD COLUMN diarization_provider TEXT;
ALTER TABLE transcripts ADD COLUMN diarization_model TEXT;
ALTER TABLE transcripts ADD COLUMN diarization_model_revision TEXT;
ALTER TABLE transcripts ADD COLUMN diarization_revision INTEGER;
ALTER TABLE transcripts ADD COLUMN diarization_window_id TEXT;
ALTER TABLE transcripts ADD COLUMN diarization_window_start_frame INTEGER;
ALTER TABLE transcripts ADD COLUMN diarization_window_end_frame INTEGER;
ALTER TABLE transcripts ADD COLUMN diarization_status TEXT;
ALTER TABLE transcripts ADD COLUMN diarization_latency_ms INTEGER;

ALTER TABLE utterance_revisions ADD COLUMN diarization_provider TEXT;
ALTER TABLE utterance_revisions ADD COLUMN diarization_model TEXT;
ALTER TABLE utterance_revisions ADD COLUMN diarization_model_revision TEXT;
ALTER TABLE utterance_revisions ADD COLUMN diarization_revision INTEGER;
ALTER TABLE utterance_revisions ADD COLUMN diarization_window_id TEXT;
ALTER TABLE utterance_revisions ADD COLUMN diarization_window_start_frame INTEGER;
ALTER TABLE utterance_revisions ADD COLUMN diarization_window_end_frame INTEGER;
ALTER TABLE utterance_revisions ADD COLUMN diarization_status TEXT;
ALTER TABLE utterance_revisions ADD COLUMN diarization_latency_ms INTEGER;

CREATE INDEX IF NOT EXISTS idx_transcripts_meeting_diarization_window
    ON transcripts(meeting_id, diarization_window_id)
    WHERE diarization_window_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_utterance_revisions_diarization_window
    ON utterance_revisions(meeting_id, diarization_window_id, diarization_revision)
    WHERE diarization_window_id IS NOT NULL;
