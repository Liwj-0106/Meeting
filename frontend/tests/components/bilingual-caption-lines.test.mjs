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
const componentPath = path.join(
  testDirectory,
  '..',
  '..',
  'src',
  'components',
  'BilingualCaptionLines.tsx',
);
const output = ts.transpileModule(fs.readFileSync(componentPath, 'utf8'), {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS,
    target: ts.ScriptTarget.ES2020,
    jsx: ts.JsxEmit.ReactJSX,
    esModuleInterop: true,
  },
}).outputText;
const componentModule = { exports: {} };
vm.runInNewContext(output, {
  exports: componentModule.exports,
  module: componentModule,
  require,
});

const { BilingualCaptionLines } = componentModule.exports;
let markup = renderToStaticMarkup(React.createElement(BilingualCaptionLines, {
  fontSize: 30,
  lines: [{
    id: 'line-ready',
    original: '会議を始めます。',
    translation: '我们开始开会。',
    translationState: 'ready',
  }],
}));
assert.ok(markup.indexOf('会議を始めます。') < markup.indexOf('我们开始开会。'));
assert.match(markup, /#A7F3E8/i);
assert.match(markup, /font-size:30px/);

markup = renderToStaticMarkup(React.createElement(BilingualCaptionLines, {
  fontSize: 24,
  lines: [{ id: 'line-pending', original: 'Hello', translationState: 'pending' }],
}));
assert.match(markup, /Hello/);
assert.match(markup, /正在翻译…/);

markup = renderToStaticMarkup(React.createElement(BilingualCaptionLines, {
  fontSize: 24,
  lines: [{
    id: 'line-error',
    original: 'Hello',
    translationState: 'error',
    errorMessage: 'provider_internal_id: stack trace should not be visible',
  }],
}));
assert.match(markup, /译文暂不可用，请检查翻译设置/);
assert.doesNotMatch(markup, /provider_internal_id|stack trace/);
assert.match(markup, /role="status"/);

console.log('bilingual caption line tests passed');
