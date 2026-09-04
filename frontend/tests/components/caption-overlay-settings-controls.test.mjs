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
  'CaptionOverlaySettingsControls.tsx',
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

const { CaptionOverlaySettingsControls } = componentModule.exports;
const controller = (mousePassthrough = false) => ({
  settings: {
    window_width: 900,
    window_height: 180,
    font_size: 30,
    background_opacity: 80,
    mouse_passthrough: mousePassthrough,
    content_protection: false,
    assistant_mode: 'captions',
    window_x: null,
    window_y: null,
  },
  isLoading: false,
  isSaving: false,
  error: null,
  updateSettings: () => {},
  resetSettings: () => {},
});

let markup = renderToStaticMarkup(React.createElement(CaptionOverlaySettingsControls, {
  controller: controller(),
  tone: 'dark',
}));

assert.match(markup, /悬浮会议助手/);
assert.match(markup, /默认显示模式/);
assert.match(markup, />字幕</);
assert.match(markup, />要点</);
assert.match(markup, />待办</);
assert.match(markup, /窗口宽度/);
assert.match(markup, /360 像素/);
assert.match(markup, /1600 像素/);
assert.match(markup, /窗口高度/);
assert.match(markup, /100 像素/);
assert.match(markup, /480 像素/);
assert.match(markup, /内容字号/);
assert.match(markup, /16 像素/);
assert.match(markup, /56 像素/);
assert.match(markup, /背景不透明度/);
assert.match(markup, /0%/);
assert.match(markup, /100%/);
assert.match(markup, /开启前请确认：悬浮窗将不再响应鼠标/);
assert.match(markup, /当前主窗口设置和系统托盘始终可以关闭穿透/);
assert.match(markup, /aria-label="开启鼠标穿透"/);
assert.match(markup, /aria-describedby="mouse-passthrough-help"/);
assert.doesNotMatch(markup.replace(/<[^>]+>/g, ' '), /\bpx\b/i);

markup = renderToStaticMarkup(React.createElement(CaptionOverlaySettingsControls, {
  controller: controller(true),
}));
assert.match(markup, /aria-label="关闭鼠标穿透"/);
assert.match(markup, /aria-checked="true"/);

console.log('caption overlay settings control tests passed');
