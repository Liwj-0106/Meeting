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
  'translation-events.ts',
);
const require = createRequire(import.meta.url);
const sourceCode = fs.readFileSync(modulePath, 'utf8');
const compiled = ts.transpileModule(sourceCode, {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS,
    target: ts.ScriptTarget.ES2020,
  },
}).outputText;
const module = { exports: {} };
vm.runInNewContext(compiled, { exports: module.exports, module, require });

const {
  createTranslationReducerState,
  reduceTranslationEvent,
  registerTranslationSource,
  selectVisibleTranslations,
} = module.exports;

const HASH_A = 'a'.repeat(64);
const HASH_B = 'b'.repeat(64);

const source = (overrides = {}) => ({
  meeting_id: 'meeting-1',
  event_id: 'source-partial',
  utterance_id: 'utterance-1',
  revision: 0,
  text_hash: HASH_A,
  event_kind: 'partial',
  is_stable: false,
  source_language: 'ja',
  speaker_id: 'speaker-1',
  ...overrides,
});

const translation = (boundSource, overrides = {}) => ({
  schema_version: 1,
  translation_event_id: 'translation-1',
  source: {
    meeting_id: boundSource.meeting_id,
    event_id: boundSource.event_id,
    utterance_id: boundSource.utterance_id,
    revision: boundSource.revision,
    text_hash: boundSource.text_hash,
  },
  source_kind: boundSource.event_kind,
  translation_revision: 1,
  generation: 1,
  event_kind: 'snapshot',
  status: 'partial',
  request_fingerprint: {
    source_language: boundSource.source_language,
    target_language: 'zh-CN',
    provider: 'deterministic-fake',
    model: 'fixture-model',
  },
  source_language: boundSource.source_language,
  target_language: 'zh-CN',
  translated_text: '你好',
  provider: 'deterministic-fake',
  model: 'fixture-model',
  speaker_id: boundSource.speaker_id,
  latency_ms: 20,
  created_at_ms: 20,
  ...overrides,
});

let state = createTranslationReducerState();
assert.equal(registerTranslationSource(state, null).outcome, 'invalid');
assert.equal(reduceTranslationEvent(state, null).outcome, 'invalid');
assert.equal(
  reduceTranslationEvent(state, { ...translation(source()), source: null }).outcome,
  'invalid',
  'null source bindings fail closed without throwing',
);
const { source: _omittedSource, ...translationWithoutSource } = translation(source());
assert.equal(
  reduceTranslationEvent(state, translationWithoutSource).outcome,
  'invalid',
  'missing source bindings fail closed without throwing',
);
let result = registerTranslationSource(state, source());
assert.equal(result.outcome, 'applied');
state = result.state;

const partialTranslation = translation(source());
result = reduceTranslationEvent(state, partialTranslation);
assert.equal(result.outcome, 'applied');
state = result.state;
assert.equal(selectVisibleTranslations(state)[0].translated_text, '你好');

assert.equal(
  reduceTranslationEvent(state, partialTranslation).outcome,
  'duplicate',
  'identical delivery ID is idempotent',
);
assert.equal(
  reduceTranslationEvent(state, { ...partialTranslation, translated_text: '冲突内容' }).outcome,
  'event_id_conflict',
  'one delivery ID cannot be reused with another payload',
);

const finalSource = source({
  event_id: 'source-final',
  revision: 1,
  text_hash: HASH_B,
  event_kind: 'final',
  is_stable: true,
});
result = registerTranslationSource(state, finalSource);
assert.equal(result.outcome, 'applied');
state = result.state;
assert.equal(
  selectVisibleTranslations(state).length,
  0,
  'a source correction immediately hides translation bound to the old triple',
);

const delayedOldTranslation = translation(source(), {
  translation_event_id: 'translation-delayed-old',
  translation_revision: 2,
  generation: 2,
});
assert.equal(
  reduceTranslationEvent(state, delayedOldTranslation).outcome,
  'unbound',
  'a high translation revision cannot bypass a stale source binding',
);

for (const mismatchedField of ['event_id', 'revision', 'text_hash']) {
  const invalidBinding = translation(finalSource, {
    translation_event_id: `translation-bad-${mismatchedField}`,
    source: {
      meeting_id: finalSource.meeting_id,
      event_id: finalSource.event_id,
      utterance_id: finalSource.utterance_id,
      revision: finalSource.revision,
      text_hash: finalSource.text_hash,
      [mismatchedField]: mismatchedField === 'revision'
        ? finalSource.revision + 1
        : (mismatchedField === 'text_hash' ? HASH_A : 'another-event'),
    },
    status: 'final',
  });
  assert.equal(
    reduceTranslationEvent(state, invalidBinding).outcome,
    'unbound',
    `${mismatchedField} participates in the source gate`,
  );
}

const finalTranslation = translation(finalSource, {
  translation_event_id: 'translation-final',
  translation_revision: 2,
  generation: 2,
  status: 'final',
  created_at_ms: 40,
});
result = reduceTranslationEvent(state, finalTranslation);
assert.equal(result.outcome, 'applied');
state = result.state;

assert.equal(
  reduceTranslationEvent(state, {
    ...finalTranslation,
    translation_event_id: 'translation-same-generation',
    translation_revision: 3,
  }).outcome,
  'stale',
  'a larger translation revision cannot reuse the current generation',
);
assert.equal(
  reduceTranslationEvent(state, {
    ...finalTranslation,
    translation_event_id: 'translation-same-revision',
    generation: 3,
  }).outcome,
  'stale',
  'a larger generation cannot reuse the current translation revision',
);

assert.equal(
  reduceTranslationEvent(state, {
    ...finalTranslation,
    translation_event_id: 'translation-final-with-reuse-edge',
    translation_revision: 3,
    generation: 3,
    reused_from_event_id: 'translation-final',
  }).outcome,
  'invalid',
  'only reused snapshots may carry reused_from_event_id',
);

const failedWithoutProvider = {
  ...finalTranslation,
  translation_event_id: 'translation-failed-without-provider',
  translation_revision: 3,
  generation: 3,
  event_kind: 'error',
  status: 'failed',
  translated_text: undefined,
  provider: undefined,
  latency_ms: undefined,
  error: {
    code: 'provider_failed',
    message: 'safe failure',
    retryable: true,
  },
};
assert.equal(
  reduceTranslationEvent(state, failedWithoutProvider).outcome,
  'invalid',
  'failed events require their bound provider',
);
assert.equal(
  reduceTranslationEvent(state, { ...failedWithoutProvider, provider: 'deterministic-fake', error: null }).outcome,
  'invalid',
  'null errors fail closed without throwing',
);

const olderGeneration = translation(finalSource, {
  translation_event_id: 'translation-old-generation',
  translation_revision: 3,
  generation: 1,
  status: 'final',
  translated_text: '迟到旧译文',
});
assert.equal(
  reduceTranslationEvent(state, olderGeneration).outcome,
  'stale',
  'generation gate rejects a canceled request even with a larger result revision',
);

const tokenDelta = translation(finalSource, {
  translation_event_id: 'translation-token-delta',
  translation_revision: 3,
  generation: 3,
  token_delta: '你',
});
assert.equal(
  reduceTranslationEvent(state, tokenDelta).outcome,
  'invalid',
  'durable reducer accepts complete snapshots, not provider token deltas',
);

const speakerSource = source({
  event_id: 'source-speaker-update',
  revision: 2,
  text_hash: HASH_B,
  event_kind: 'speaker_update',
  is_stable: true,
  speaker_id: 'speaker-2',
});
result = registerTranslationSource(state, speakerSource);
assert.equal(result.outcome, 'applied');
state = result.state;
const reusedTranslation = translation(speakerSource, {
  translation_event_id: 'translation-reused',
  translation_revision: 3,
  generation: 3,
  status: 'reused',
  reused_from_event_id: 'translation-final',
  speaker_id: 'speaker-2',
});
result = reduceTranslationEvent(state, reusedTranslation);
assert.equal(result.outcome, 'applied');
state = result.state;
assert.equal(selectVisibleTranslations(state)[0].status, 'reused');

const retractedSource = source({
  event_id: 'source-retraction',
  revision: 3,
  text_hash: HASH_A,
  event_kind: 'retraction',
  is_stable: true,
});
result = registerTranslationSource(state, retractedSource);
assert.equal(result.outcome, 'applied');
state = result.state;
assert.equal(selectVisibleTranslations(state).length, 0);
assert.equal(
  reduceTranslationEvent(state, {
    ...reusedTranslation,
    translation_event_id: 'translation-late-after-retraction',
    translation_revision: 4,
    generation: 4,
  }).outcome,
  'unbound',
  'late text cannot resurrect a retracted source',
);

const translationRetraction = translation(retractedSource, {
  translation_event_id: 'translation-retraction',
  translation_revision: 4,
  generation: 4,
  event_kind: 'retraction',
  status: 'retracted',
  translated_text: undefined,
  provider: undefined,
  model: undefined,
  latency_ms: undefined,
});
result = reduceTranslationEvent(state, translationRetraction);
assert.equal(result.outcome, 'applied');
state = result.state;
assert.equal(selectVisibleTranslations(state).length, 0);

const sameRevisionFinal = source({
  event_id: 'source-same-revision-partial',
  utterance_id: 'utterance-2',
  revision: 0,
});
state = registerTranslationSource(state, sameRevisionFinal).state;
result = reduceTranslationEvent(state, translation(sameRevisionFinal, {
  translation_event_id: 'translation-same-revision-partial',
}));
assert.equal(result.outcome, 'applied');
state = result.state;
const sameRevisionStableFinal = {
  ...sameRevisionFinal,
  event_id: 'source-same-revision-final',
  event_kind: 'final',
  is_stable: true,
};
result = registerTranslationSource(state, {
  ...sameRevisionStableFinal,
});
assert.equal(result.outcome, 'applied', 'stable final outranks partial at one source revision');
state = result.state;
result = reduceTranslationEvent(state, translation(sameRevisionStableFinal, {
  translation_event_id: 'translation-same-revision-final',
  translation_revision: 2,
  generation: 2,
  status: 'final',
}));
assert.equal(result.outcome, 'applied', 'same-revision final translation replaces partial');
state = result.state;
assert.equal(
  reduceTranslationEvent(state, translation(sameRevisionFinal, {
    translation_event_id: 'translation-same-revision-partial-late',
    translation_revision: 3,
    generation: 3,
  })).outcome,
  'unbound',
  'late lower-priority source translation cannot roll the final back',
);
assert.equal(
  registerTranslationSource(state, {
    ...sameRevisionFinal,
    event_id: 'source-conflicting-partial',
  }).outcome,
  'stale',
  'a delayed same-revision partial cannot replace the final',
);
assert.equal(
  reduceTranslationEvent(state, translation(sameRevisionStableFinal, {
    translation_event_id: 'translation-source-kind-mismatch',
    source_kind: 'correction',
    translation_revision: 3,
    generation: 3,
    status: 'final',
  })).outcome,
  'unbound',
  'source_kind is part of the current-source binding contract',
);

const glossarySource = source({
  event_id: 'source-glossary',
  utterance_id: 'utterance-glossary',
  revision: 1,
  event_kind: 'final',
  is_stable: true,
});
state = registerTranslationSource(state, glossarySource).state;
const glossaryV1 = {
  glossary_id: 'meeting-terms',
  version: 1,
  content_hash: HASH_A,
};
const glossaryV2 = {
  glossary_id: 'meeting-terms',
  version: 2,
  content_hash: HASH_B,
};
const glossaryTranslationV1 = translation(glossarySource, {
  translation_event_id: 'translation-glossary-v1',
  glossary: glossaryV1,
  request_fingerprint: {
    source_language: 'ja',
    target_language: 'zh-CN',
    provider: 'deterministic-fake',
    model: 'fixture-model',
    glossary: glossaryV1,
  },
  status: 'final',
});
result = reduceTranslationEvent(state, glossaryTranslationV1);
assert.equal(result.outcome, 'applied');
state = result.state;

const glossaryTranslationV2 = translation(glossarySource, {
  translation_event_id: 'translation-glossary-v2',
  translation_revision: 2,
  generation: 2,
  glossary: glossaryV2,
  request_fingerprint: {
    source_language: 'ja',
    target_language: 'zh-CN',
    provider: 'deterministic-fake',
    model: 'fixture-model',
    glossary: glossaryV2,
  },
  status: 'final',
});
result = reduceTranslationEvent(state, glossaryTranslationV2);
assert.equal(result.outcome, 'applied', 'a newer glossary fingerprint is a new translation');
state = result.state;
assert.equal(
  reduceTranslationEvent(state, {
    ...glossaryTranslationV1,
    translation_event_id: 'translation-glossary-v1-late',
    translation_revision: 3,
  }).outcome,
  'stale',
  'a late old-glossary generation cannot replace the new fingerprint',
);
assert.equal(
  reduceTranslationEvent(state, {
    ...glossaryTranslationV2,
    translation_event_id: 'translation-glossary-mismatch',
    translation_revision: 3,
    generation: 3,
    glossary: glossaryV1,
  }).outcome,
  'invalid',
  'flat glossary columns must match the request fingerprint',
);

console.log('translation event reducer tests passed');
