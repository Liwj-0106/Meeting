import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import { fileURLToPath } from 'node:url';

const testDirectory = path.dirname(fileURLToPath(import.meta.url));
const sourcePath = path.join(testDirectory, '..', '..', 'src', 'lib', 'live-summary-overlay.ts');
const output = ts.transpileModule(fs.readFileSync(sourcePath, 'utf8'), {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS,
    target: ts.ScriptTarget.ES2020,
    esModuleInterop: true,
  },
}).outputText;
const reducerModule = { exports: {} };
vm.runInNewContext(output, {
  exports: reducerModule.exports,
  module: reducerModule,
  Set,
  Number,
});

const {
  bindLiveSummarySnapshot,
  countAssistantEvidence,
  createLiveSummaryOverlayState,
  reduceLiveSummaryOverlayEvent,
  selectAssistantItems,
} = reducerModule.exports;

const HASH = 'a'.repeat(64);
const scope = (kind, id) => ({ kind, id });
const evidence = (summaryScope, eventId = 'event-1') => ({
  scope: summaryScope,
  utterance_id: 'utterance-1',
  source_revision: 1,
  source_event_id: eventId,
  source_text_hash: HASH,
});
const item = (summaryScope, overrides = {}) => ({
  item_id: `item-${overrides.kind ?? 'topic'}`,
  kind: 'topic',
  title: '缓存策略',
  body: '团队正在比较两种缓存失效方案。',
  status: 'active',
  evidence: [evidence(summaryScope)],
  ...overrides,
});
const snapshot = (meetingId = 'meeting-a', overrides = {}) => ({
  schemaVersion: 1,
  scope: scope('meeting', meetingId),
  meetingId,
  lifecycle: 'active',
  dispatchState: 'idle',
  provider: { available: true, provider: 'deterministic-fake', model: 'fake-v1' },
  transcriptCursor: 2,
  summaryRevision: 1,
  generation: 1,
  finalized: false,
  recovered: false,
  updatedAt: '2026-09-02T08:30:00Z',
  items: [item(scope('meeting', meetingId))],
  ...overrides,
});
const sessionSnapshot = (sessionScopeId = 'recording-summary-a', overrides = {}) => ({
  schemaVersion: 1,
  scope: scope('recording_session', sessionScopeId),
  sessionScopeId,
  lifecycle: 'active',
  dispatchState: 'idle',
  provider: { available: true, provider: 'deterministic-fake', model: 'fake-v1' },
  transcriptCursor: 2,
  summaryRevision: 1,
  generation: 1,
  finalized: false,
  recovered: false,
  updatedAt: '2026-09-02T08:30:00Z',
  items: [item(scope('recording_session', sessionScopeId))],
  ...overrides,
});
const event = (kind = 'summary_committed', nextSnapshot = snapshot()) => ({
  schemaVersion: 1,
  kind,
  snapshot: nextSnapshot,
});

let state = createLiveSummaryOverlayState();
let reduction = reduceLiveSummaryOverlayEvent(state, null);
assert.equal(reduction.outcome, 'invalid');
assert.equal(reduction.state.snapshot, null);

reduction = reduceLiveSummaryOverlayEvent(state, event());
assert.equal(reduction.outcome, 'scope_mismatch', 'a late commit cannot establish an empty overlay');

reduction = reduceLiveSummaryOverlayEvent(state, event('session_started'));
assert.equal(reduction.outcome, 'applied');
state = reduction.state;
assert.equal(bindLiveSummarySnapshot(state, null).status, 'missing_transcript_scope');
assert.equal(bindLiveSummarySnapshot(state, 'meeting-b').status, 'scope_mismatch');
assert.equal(bindLiveSummarySnapshot(state, 'meeting-a').status, 'bound');

reduction = reduceLiveSummaryOverlayEvent(state, event());
assert.equal(reduction.outcome, 'applied');
state = reduction.state;
reduction = reduceLiveSummaryOverlayEvent(state, event());
assert.equal(reduction.outcome, 'duplicate');

reduction = reduceLiveSummaryOverlayEvent(state, event('transcript_accepted', snapshot('meeting-a', {
  generation: 0,
  summaryRevision: 50,
  transcriptCursor: 50,
})));
assert.equal(reduction.outcome, 'stale');
assert.equal(reduction.state, state);

reduction = reduceLiveSummaryOverlayEvent(state, event('summary_failed', snapshot('meeting-a', {
  lifecycle: 'error',
  error: { code: 'safe_error', message: '暂不可用', retryable: true },
})));
assert.equal(reduction.outcome, 'stale', 'a late lower-priority failure cannot replace a commit');

let failedState = reduceLiveSummaryOverlayEvent(
  createLiveSummaryOverlayState(),
  event('session_started', snapshot('meeting-a')),
).state;
reduction = reduceLiveSummaryOverlayEvent(failedState, event('summary_failed', snapshot('meeting-a', {
  lifecycle: 'error',
  error: { code: 'safe_error', message: '暂不可用', retryable: true },
})));
assert.equal(reduction.outcome, 'applied', 'same-revision failure is still a meaningful state transition');
failedState = reduction.state;
reduction = reduceLiveSummaryOverlayEvent(failedState, event('summary_committed', snapshot('meeting-a')));
assert.equal(reduction.outcome, 'applied', 'a commit wins over a same-revision failure');

reduction = reduceLiveSummaryOverlayEvent(state, event('summary_committed', snapshot('meeting-b')));
assert.equal(reduction.outcome, 'scope_mismatch');
assert.equal(reduction.state, state);

reduction = reduceLiveSummaryOverlayEvent(state, event('session_started', snapshot('meeting-b', {
  generation: 0,
  summaryRevision: 0,
  transcriptCursor: 0,
  items: [],
})));
assert.equal(reduction.outcome, 'applied');
assert.equal(reduction.state.snapshot.meetingId, 'meeting-b');

const filteredSnapshot = snapshot('meeting-a', {
  items: [
    item(scope('meeting', 'meeting-a'), { item_id: 'topic-1', kind: 'topic' }),
    item(scope('meeting', 'meeting-a'), { item_id: 'decision-1', kind: 'decision' }),
    item(scope('meeting', 'meeting-a'), { item_id: 'action-1', kind: 'action_item' }),
    item(scope('meeting', 'meeting-a'), { item_id: 'risk-retracted', kind: 'risk', status: 'retracted' }),
  ],
});
const highlights = selectAssistantItems(filteredSnapshot, 'highlights');
const actions = selectAssistantItems(filteredSnapshot, 'actions');
assert.deepEqual(Array.from(highlights, (entry) => entry.item_id), ['topic-1', 'decision-1']);
assert.deepEqual(Array.from(actions, (entry) => entry.item_id), ['action-1']);
assert.equal(countAssistantEvidence(highlights), 1, 'the evidence rail counts unique source revisions');

const invalidEvidenceEvent = event('summary_committed', snapshot('meeting-a', {
  items: [item(scope('meeting', 'meeting-a'), {
    evidence: [{ ...evidence(scope('meeting', 'meeting-a')), source_text_hash: 'not-a-hash' }],
  })],
}));
assert.equal(reduceLiveSummaryOverlayEvent(state, invalidEvidenceEvent).outcome, 'invalid');

let sessionState = createLiveSummaryOverlayState();
reduction = reduceLiveSummaryOverlayEvent(
  sessionState,
  event('session_started', sessionSnapshot()),
);
assert.equal(reduction.outcome, 'applied');
sessionState = reduction.state;
assert.equal(bindLiveSummarySnapshot(sessionState, 'audio-session-private').status, 'scope_mismatch');
assert.equal(bindLiveSummarySnapshot(sessionState, 'recording-summary-a').status, 'bound');

reduction = reduceLiveSummaryOverlayEvent(
  sessionState,
  event('summary_committed', sessionSnapshot('recording-summary-b')),
);
assert.equal(reduction.outcome, 'scope_mismatch', 'another recording cannot replace the active scope');

const invalidSessionShape = sessionSnapshot('recording-summary-a', { meetingId: 'fake-meeting' });
assert.equal(
  reduceLiveSummaryOverlayEvent(
    createLiveSummaryOverlayState(),
    event('session_started', invalidSessionShape),
  ).outcome,
  'invalid',
);

console.log('live summary overlay reducer tests passed');
