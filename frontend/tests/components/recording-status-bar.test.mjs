import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const testDirectory = path.dirname(fileURLToPath(import.meta.url));

function compileCommonJs(file, jsx = false) {
  const source = fs.readFileSync(file, 'utf8');
  return ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2020,
      jsx: jsx ? ts.JsxEmit.ReactJSX : undefined,
      esModuleInterop: true,
    },
  }).outputText;
}

const healthModule = { exports: {} };
const healthPath = path.join(testDirectory, '..', '..', 'src', 'lib', 'audio-source-health.ts');
vm.runInNewContext(compileCommonJs(healthPath), {
  exports: healthModule.exports,
  module: healthModule,
  require,
});

const asrHealthModule = { exports: {} };
const asrHealthPath = path.join(testDirectory, '..', '..', 'src', 'lib', 'asr-health.ts');
vm.runInNewContext(compileCommonJs(asrHealthPath), {
  exports: asrHealthModule.exports,
  module: asrHealthModule,
  require,
});

let mockedRecordingState;
const componentModule = { exports: {} };
const componentPath = path.join(
  testDirectory,
  '..',
  '..',
  'src',
  'components',
  'RecordingStatusBar.tsx',
);
const mockRequire = specifier => {
  if (specifier === '@/contexts/RecordingStateContext') {
    return { useRecordingState: () => mockedRecordingState };
  }
  if (specifier === '@/lib/audio-source-health') {
    return healthModule.exports;
  }
  if (specifier === '@/lib/asr-health') {
    return asrHealthModule.exports;
  }
  if (specifier === 'framer-motion') {
    return {
      motion: {
        div: ({ initial, animate, exit, transition, children, ...props }) => (
          React.createElement('div', props, children)
        ),
      },
      useReducedMotion: () => true,
    };
  }
  return require(specifier);
};
vm.runInNewContext(compileCommonJs(componentPath, true), {
  exports: componentModule.exports,
  module: componentModule,
  require: mockRequire,
});

const { RecordingStatusBar } = componentModule.exports;
const sourceEvent = (overrides = {}) => ({
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

mockedRecordingState = {
  activeDuration: 65,
  isRecording: true,
  asrHealth: {
    schema_version: 1,
    event_id: 'asr-streaming',
    session_id: 'session-1',
    sequence: 1,
    provider: 'deepgram',
    kind: 'connection_changed',
    state: 'streaming',
    severity: 'info',
    code: 'streaming',
    recoverable: true,
    attempt: 0,
    at_frame: 480,
    replay_from_frame: null,
    dropped_frames: 0,
    detail: null,
    created_at: '2026-09-01T00:00:00.000Z',
  },
  sourceHealth: {
    microphone: sourceEvent(),
    system: sourceEvent({
      event_id: 'system-recovering',
      sequence: 2,
      track: 'system',
      state: 'recovering',
      severity: 'warning',
    }),
    mixed: sourceEvent({
      event_id: 'mixed-healthy',
      sequence: 3,
      track: 'mixed',
    }),
  },
};
let markup = renderToStaticMarkup(React.createElement(RecordingStatusBar));
assert.match(markup, /正在录音/);
assert.match(markup, /麦克风正常/);
assert.match(markup, /媒体重连中/);
assert.match(markup, /Deepgram实时识别/);
assert.doesNotMatch(markup, /混合流正常/, 'healthy mixed state should not duplicate direct sources');
assert.match(markup, /role="status"/);
assert.match(markup, /aria-live="polite"/);

mockedRecordingState = {
  ...mockedRecordingState,
  sourceHealth: {
    ...mockedRecordingState.sourceHealth,
    mixed: sourceEvent({
      event_id: 'mixed-fatal',
      sequence: 4,
      track: 'mixed',
      kind: 'fatal_error',
      state: 'failed',
      severity: 'fatal',
      detail: 'Audio pipeline stopped',
    }),
  },
};
markup = renderToStaticMarkup(React.createElement(RecordingStatusBar));
assert.match(markup, /混合流严重异常/);
assert.match(markup, /role="alert"/);
assert.match(markup, /aria-live="assertive"/);

console.log('recording status bar tests passed');
