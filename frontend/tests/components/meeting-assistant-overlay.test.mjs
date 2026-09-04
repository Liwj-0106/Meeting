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

function transpile(sourcePath) {
  return ts.transpileModule(fs.readFileSync(sourcePath, 'utf8'), {
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2020,
      jsx: ts.JsxEmit.ReactJSX,
      esModuleInterop: true,
    },
  }).outputText;
}

const reducerPath = path.join(testDirectory, '..', '..', 'src', 'lib', 'live-summary-overlay.ts');
const reducerModule = { exports: {} };
vm.runInNewContext(transpile(reducerPath), {
  exports: reducerModule.exports,
  module: reducerModule,
  Set,
  Number,
});

function loadComponent(relativePath) {
  const componentModule = { exports: {} };
  const componentPath = path.join(testDirectory, '..', '..', 'src', 'components', relativePath);
  const componentRequire = (specifier) => {
    if (specifier === '@/lib/live-summary-overlay') return reducerModule.exports;
    return require(specifier);
  };
  vm.runInNewContext(transpile(componentPath), {
    exports: componentModule.exports,
    module: componentModule,
    require: componentRequire,
    Intl,
    Date,
    Number,
    Array,
  });
  return componentModule.exports;
}

const { MeetingAssistantModeSwitch } = loadComponent('MeetingAssistantModeSwitch.tsx');
const { MeetingAssistantPanel } = loadComponent('MeetingAssistantPanel.tsx');

let markup = renderToStaticMarkup(React.createElement(MeetingAssistantModeSwitch, {
  mode: 'highlights',
  onModeChange: () => {},
}));
assert.match(markup, /role="tablist"/);
assert.match(markup, /aria-label="会议助手显示模式"/);
assert.match(markup, /字幕模式，快捷键 Alt\+1/);
assert.match(markup, /要点模式，快捷键 Alt\+2/);
assert.match(markup, /待办模式，快捷键 Alt\+3/);
assert.match(markup, /aria-selected="true"[^>]*tabindex="0"/);

markup = renderToStaticMarkup(React.createElement(MeetingAssistantPanel, {
  mode: 'highlights',
  fontSize: 30,
  binding: { status: 'missing_transcript_scope' },
}));
assert.match(markup, /尚未建立当前录音会话/);
assert.match(markup, /不会显示上一场会议的内容/);

const hash = 'b'.repeat(64);
const makeEvidence = (eventId) => ({
  meeting_id: 'meeting-current',
  utterance_id: `utterance-${eventId}`,
  source_revision: 1,
  source_event_id: eventId,
  source_text_hash: hash,
});
const snapshot = {
  schemaVersion: 1,
  meetingId: 'meeting-current',
  lifecycle: 'active',
  dispatchState: 'idle',
  provider: { available: true, provider: 'fake' },
  transcriptCursor: 3,
  summaryRevision: 2,
  generation: 2,
  finalized: false,
  recovered: false,
  updatedAt: '2026-09-02T08:30:00Z',
  items: [
    {
      item_id: 'decision-1',
      kind: 'decision',
      title: '采用两级缓存',
      body: '先上线本地缓存，再观察命中率。',
      status: 'active',
      evidence: [makeEvidence('event-1'), makeEvidence('event-2')],
    },
    {
      item_id: 'risk-retracted',
      kind: 'risk',
      title: '已经撤回的风险',
      body: '不应出现在悬浮窗。',
      status: 'retracted',
      evidence: [makeEvidence('event-3')],
    },
    {
      item_id: 'action-1',
      kind: 'action_item',
      title: '补充压测数据',
      body: '周五前完成。',
      owner: '小林',
      due_at: '2026-09-04',
      status: 'needs_review',
      evidence: [makeEvidence('event-4')],
    },
  ],
};

markup = renderToStaticMarkup(React.createElement(MeetingAssistantPanel, {
  mode: 'highlights',
  fontSize: 30,
  binding: { status: 'bound', snapshot },
}));
assert.match(markup, /采用两级缓存/);
assert.doesNotMatch(markup, /已经撤回的风险/);
assert.doesNotMatch(markup, /补充压测数据/);
assert.match(markup, /证据 2/);
assert.match(markup, /#34D399/i);

markup = renderToStaticMarkup(React.createElement(MeetingAssistantPanel, {
  mode: 'actions',
  fontSize: 28,
  binding: { status: 'bound', snapshot },
}));
assert.match(markup, /补充压测数据/);
assert.match(markup, /负责人 小林/);
assert.match(markup, /截止 2026-09-04/);
assert.match(markup, /待确认/);
assert.doesNotMatch(markup, /采用两级缓存/);
assert.doesNotMatch(markup, /已经撤回的风险/);

console.log('meeting assistant overlay component tests passed');
