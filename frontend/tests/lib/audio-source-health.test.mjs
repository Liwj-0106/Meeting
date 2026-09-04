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
  'audio-source-health.ts',
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
  createInitialAudioSourceHealthState,
  deriveOverallAudioHealth,
  listAudioSourceHealth,
  presentAudioSourceHealth,
  reduceAudioSourceHealth,
  reduceAudioSourceHealthEvents,
} = module.exports;

const event = (overrides = {}) => ({
  schema_version: 1,
  event_id: 'event-1',
  session_id: 'session-1',
  sequence: 1,
  track: 'microphone',
  kind: 'state_changed',
  state: 'healthy',
  severity: 'info',
  code: 'audio_state_changed',
  recoverable: true,
  attempt: 0,
  at_frame: 480,
  end_frame: null,
  generation: 0,
  gap_frames: 0,
  device_label: null,
  detail: null,
  created_at: '2026-09-01T00:00:00.000Z',
  ...overrides,
});

let state = createInitialAudioSourceHealthState();
state = reduceAudioSourceHealth(state, event());
assert.equal(state.byTrack.microphone.state, 'healthy');
assert.equal(deriveOverallAudioHealth(state.byTrack), 'healthy');

const duplicate = reduceAudioSourceHealth(
  state,
  event({ event_id: 'event-1', sequence: 2, track: 'system', state: 'degraded' }),
);
assert.equal(duplicate, state, 'event IDs must be globally idempotent');
assert.equal(duplicate.byTrack.system, undefined);

const oldSequence = reduceAudioSourceHealth(
  state,
  event({ event_id: 'old-sequence', sequence: 1, state: 'interrupted' }),
);
assert.equal(oldSequence, state, 'same-track sequence must increase');

state = reduceAudioSourceHealth(
  state,
  event({ event_id: 'new-generation', sequence: 3, generation: 2, state: 'recovering' }),
);
assert.equal(state.byTrack.microphone.generation, 2);

const oldGeneration = reduceAudioSourceHealth(
  state,
  event({ event_id: 'old-generation', sequence: 4, generation: 1, state: 'healthy' }),
);
assert.equal(oldGeneration, state, 'a later delivery cannot restore an old stream generation');
assert.equal(deriveOverallAudioHealth(state.byTrack), 'degraded');
assert.equal(presentAudioSourceHealth(state.byTrack.microphone).label, '麦克风重连中');

state = reduceAudioSourceHealth(
  state,
  event({
    event_id: 'future-track',
    sequence: 5,
    track: 'browser_tab',
    state: 'throttled',
    severity: 'notice',
    device_label: '浏览器标签页',
  }),
);
assert.equal(state.byTrack.browser_tab.state, 'throttled', 'unknown protocol values are retained');
assert.equal(presentAudioSourceHealth(state.byTrack.browser_tab).label, '浏览器标签页状态已更新');

state = reduceAudioSourceHealth(
  state,
  event({
    event_id: 'system-warning',
    sequence: 6,
    track: 'system',
    state: 'interrupted',
    severity: 'warning',
  }),
);
assert.equal(
  listAudioSourceHealth(state.byTrack).map(item => item.track).join(','),
  'microphone,system,browser_tab',
  'known tracks are presented first in stable order',
);

state = reduceAudioSourceHealth(
  state,
  event({
    event_id: 'fatal-mixed',
    sequence: 7,
    track: 'mixed',
    kind: 'fatal_error',
    state: 'failed',
    severity: 'error',
  }),
);
assert.equal(deriveOverallAudioHealth(state.byTrack), 'fatal', 'fatal kind is authoritative');
assert.equal(presentAudioSourceHealth(state.byTrack.mixed).tone, 'fatal');

const staleOtherSession = reduceAudioSourceHealth(
  state,
  event({ event_id: 'stale-session', session_id: 'session-old', sequence: 8 }),
);
assert.equal(staleOtherSession, state, 'a delayed event cannot replace the active session');

const nextSession = reduceAudioSourceHealth(
  state,
  event({
    event_id: 'next-session',
    session_id: 'session-2',
    sequence: 0,
    generation: 0,
    track: 'mixed',
    kind: 'session_started',
  }),
);
assert.equal(nextSession.sessionId, 'session-2');
assert.deepEqual(Object.keys(nextSession.byTrack), ['mixed']);

const persistedSnapshot = [
  event({
    event_id: 'reload-started',
    session_id: 'reload-session',
    sequence: 0,
    track: 'mixed',
    kind: 'session_started',
    state: 'starting',
  }),
  event({
    event_id: 'reload-mic-healthy',
    session_id: 'reload-session',
    sequence: 1,
    track: 'microphone',
  }),
];
const bufferedLiveTail = [
  persistedSnapshot[1],
  event({
    event_id: 'reload-system-recovering',
    session_id: 'reload-session',
    sequence: 2,
    track: 'system',
    state: 'recovering',
    severity: 'warning',
  }),
];
const restored = reduceAudioSourceHealthEvents(
  createInitialAudioSourceHealthState(),
  [...persistedSnapshot, ...bufferedLiveTail],
);
assert.equal(restored.sessionId, 'reload-session');
assert.equal(restored.byTrack.microphone.state, 'healthy');
assert.equal(restored.byTrack.system.state, 'recovering');
assert.equal(
  Object.keys(restored.seenEventIds).length,
  3,
  'snapshot/live overlap must remain globally idempotent',
);

console.log('audio source health reducer tests passed');
