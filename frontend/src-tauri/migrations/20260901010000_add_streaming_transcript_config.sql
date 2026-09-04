-- Stage 3: versioned online-streaming transcription settings.
--
-- API keys deliberately remain in their existing dedicated columns.  The
-- JSON column contains only non-secret provider behaviour, so changing normal
-- transcription settings never needs to read, round-trip, or overwrite a key.
ALTER TABLE transcript_settings ADD COLUMN streamingConfig TEXT;
