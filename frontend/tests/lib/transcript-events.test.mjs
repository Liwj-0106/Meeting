import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const modulePath = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  'src',
  'lib',
  'transcript-events.ts',
);
const require = createRequire(import.meta.url);
const source = fs.readFileSync(modulePath, 'utf8');
const compiled = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS,
    target: ts.ScriptTarget.ES2020,
  },
}).outputText;
const module = { exports: {} };
vm.runInNewContext(compiled, { exports: module.exports, module, require });

const {
  mergeTranscriptRevisionHistories,
  normalizeTranscriptEvent,
  transcriptEventPayloadEquals,
  upsertTranscripts,
} = module.exports;

const base = {
  text: '候选文本',
  timestamp: '12:00:00',
  source: 'Audio',
  sequence_id: 1,
  chunk_start_time: 1.2345,
  is_partial: false,
  confidence: 0.9,
  audio_start_time: 1.2345,
  audio_end_time: 2.0004,
  duration: 0.7659,
};

const normalized = normalizeTranscriptEvent({ ...base, asr_latency_ms: 12.7 });
assert.equal(normalized.start_ms, 1235, 'legacy seconds should round like Rust');
assert.equal(normalized.end_ms, 2000);
assert.equal(normalized.audio_start_time, 1.235);
assert.equal(normalized.asr_latency_ms, 13);
assert.equal(normalized.diarization, undefined, 'legacy events keep diarization absent');

const diarized = normalizeTranscriptEvent({
  ...base,
  asr_provider: 'deepgram',
  asr_model: 'nova-3',
  diarization: {
    provider: 'moss-worker',
    model: 'MOSS-Transcribe-Diarize',
    model_revision: 'fixture-sha',
    revision: 7,
    window_id: 'window-1',
    window_start_frame: 48000,
    window_end_frame: 96000,
    status: 'provisional',
  },
  // Nested metadata is canonical when flat mirrors disagree.
  diarization_provider: 'must-not-win',
  diarization_revision: 99,
});
assert.equal(diarized.asr_provider, 'deepgram');
assert.equal(diarized.asr_model, 'nova-3');
assert.equal(diarized.diarization_provider, 'moss-worker');
assert.equal(diarized.diarization_revision, 7);
assert.equal(diarized.diarization_window_start_frame, 48000);

const negative = normalizeTranscriptEvent({
  ...base,
  audio_start_time: -2,
  audio_end_time: -1,
});
assert.equal(negative.start_ms, 0, 'negative legacy timing should be clamped');
assert.equal(negative.audio_start_time, 0);

const olderSnapshot = {
  ...base,
  utterance_id: 'utterance-1',
  revision: 0,
  event_id: 'event-partial',
  event_kind: 'partial',
  is_stable: false,
  is_partial: true,
  text: '候选',
  created_at: '2026-01-01T00:00:00Z',
};
const liveFinal = {
  ...olderSnapshot,
  revision: 1,
  event_id: 'event-final',
  event_kind: 'final',
  is_stable: true,
  is_partial: false,
  text: '候选文本',
  created_at: '2026-01-01T00:00:01Z',
};
const reloadMerge = mergeTranscriptRevisionHistories(
  [olderSnapshot],
  [olderSnapshot],
  [liveFinal],
);
assert.equal(reloadMerge.length, 2, 'reload must retain every revision');
assert.equal(upsertTranscripts([], reloadMerge)[0].text, '候选文本');

const reusedEventId = mergeTranscriptRevisionHistories(
  [olderSnapshot],
  [{ ...liveFinal, event_id: olderSnapshot.event_id }],
);
assert.equal(reusedEventId.length, 1, 'a reused event_id must be globally idempotent');
assert.equal(
  upsertTranscripts([], reusedEventId)[0].text,
  '候选',
  'first event_id delivery wins across layers',
);

const secondRevision = {
  ...liveFinal,
  event_id: 'event-second',
};
const reusedOldIdAfterReplacement = {
  ...liveFinal,
  revision: 2,
  event_id: olderSnapshot.event_id,
  text: '不应接受',
  created_at: '2026-01-01T00:00:02Z',
};
const threeEventHistory = mergeTranscriptRevisionHistories(
  [olderSnapshot, secondRevision, reusedOldIdAfterReplacement],
);
assert.equal(threeEventHistory.length, 2);
assert.equal(
  upsertTranscripts([], threeEventHistory)[0].text,
  '候选文本',
  'a retired event_id cannot return as a later revision',
);

assert.equal(
  transcriptEventPayloadEquals(
    { ...liveFinal, speaker_id: 'speaker-1' },
    { ...liveFinal, speaker_id: 'speaker-2' },
  ),
  false,
  'speaker corrections are not duplicate payloads',
);
assert.equal(
  transcriptEventPayloadEquals(
    { ...liveFinal, diarization_provider: 'moss-worker', diarization_revision: 1 },
    { ...liveFinal, diarization_provider: 'moss-worker', diarization_revision: 2 },
  ),
  false,
  'diarization corrections are immutable revision payload content',
);

const retraction = {
  ...liveFinal,
  revision: 3,
  event_id: 'event-retraction',
  event_kind: 'retraction',
  text: '',
  created_at: '2026-01-01T00:00:03Z',
};
const delayedBeforeRetraction = {
  ...liveFinal,
  revision: 2,
  event_id: 'event-delayed-before-retraction',
  text: '不得复活的旧文本',
  created_at: '2026-01-01T00:00:02Z',
};
assert.equal(
  upsertTranscripts([], [liveFinal, retraction, delayedBeforeRetraction]).length,
  0,
  'a delayed older revision must not resurrect a retracted utterance',
);

const restoredAfterRetraction = {
  ...liveFinal,
  revision: 4,
  event_id: 'event-restored-after-retraction',
  event_kind: 'correction',
  text: '经更高修订明确恢复',
  created_at: '2026-01-01T00:00:04Z',
};
assert.equal(
  upsertTranscripts([], [liveFinal, retraction, restoredAfterRetraction])[0].text,
  '经更高修订明确恢复',
  'only a newer revision may restore a retracted utterance',
);

console.log('transcript event tests passed');
