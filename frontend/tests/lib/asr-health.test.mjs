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
  'asr-health.ts',
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
  createInitialAsrHealthState,
  isAsrHealthDegraded,
  presentAsrHealth,
  reduceAsrHealth,
  reduceAsrHealthEvents,
} = module.exports;

const event = (overrides = {}) => ({
  schema_version: 1,
  event_id: 'asr-1',
  session_id: 'session-1',
  sequence: 0,
  provider: 'deepgram',
  kind: 'session_starting',
  state: 'connecting',
  severity: 'info',
  code: 'asr_connecting',
  recoverable: true,
  attempt: 0,
  at_frame: 0,
  replay_from_frame: null,
  dropped_frames: 0,
  detail: null,
  created_at: '2026-09-01T00:00:00.000Z',
  ...overrides,
});

let state = createInitialAsrHealthState();
state = reduceAsrHealth(state, event());
assert.equal(state.latest.state, 'connecting');
assert.equal(presentAsrHealth(state.latest).label, 'Deepgram连接中');

const duplicate = reduceAsrHealth(state, event({ sequence: 1, state: 'streaming' }));
assert.equal(duplicate, state, 'duplicate event IDs must be idempotent');

const stale = reduceAsrHealth(state, event({ event_id: 'stale', sequence: 0 }));
assert.equal(stale, state, 'ASR sequence must increase globally');

const recovered = reduceAsrHealthEvents(state, [
  event({ event_id: 'streaming', sequence: 1, kind: 'connection_changed', state: 'streaming' }),
  event({ event_id: 'backoff', sequence: 2, kind: 'reconnect_scheduled', state: 'backoff', severity: 'warning' }),
  event({ event_id: 'replay', sequence: 3, kind: 'replay_started', state: 'replaying', replay_from_frame: 48_000 }),
]);
assert.equal(recovered.latest.state, 'replaying');
assert.equal(recovered.history.length, 4);
assert.equal(presentAsrHealth(recovered.latest).label, 'Deepgram补传音频');

const failed = reduceAsrHealth(
  recovered,
  event({ event_id: 'failed', sequence: 4, kind: 'provider_error', state: 'failed', severity: 'fatal' }),
);
assert.equal(isAsrHealthDegraded(failed.latest), true);
assert.equal(presentAsrHealth(failed.latest).tone, 'fatal');

const delayedOldSession = reduceAsrHealth(
  failed,
  event({ event_id: 'old', session_id: 'old-session', sequence: 99, kind: 'provider_error' }),
);
assert.equal(delayedOldSession, failed);

const nextSession = reduceAsrHealth(
  failed,
  event({ event_id: 'next', session_id: 'session-2', sequence: 0, kind: 'session_starting' }),
);
assert.equal(nextSession.sessionId, 'session-2');
assert.equal(nextSession.history.length, 1);

console.log('streaming ASR health reducer tests passed');
