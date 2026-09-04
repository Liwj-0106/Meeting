import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import { fileURLToPath } from 'node:url';

const testDirectory = path.dirname(fileURLToPath(import.meta.url));
const servicePath = path.join(testDirectory, '..', '..', 'src', 'services', 'liveTranslationService.ts');
const output = ts.transpileModule(fs.readFileSync(servicePath, 'utf8'), {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS,
    target: ts.ScriptTarget.ES2020,
    esModuleInterop: true,
  },
}).outputText;

const calls = [];
const listeners = new Map();
const publicSettings = {
  enabled: true,
  source_language: 'auto',
  target_language: 'zh-CN',
  provider: 'openai',
  model: 'gpt-4o-mini',
  endpoint: 'https://api.openai.com/v1',
  has_api_key: true,
};
const mockRequire = (specifier) => {
  if (specifier === '@tauri-apps/api/core') {
    return {
      invoke: async (command, args) => {
        calls.push({ command, args });
        if (command === 'api_queue_live_caption_translation') {
          return { request_id: 'request-1', generation: 1, state: 'queued' };
        }
        return publicSettings;
      },
    };
  }
  if (specifier === '@tauri-apps/api/event') {
    return {
      listen: async (eventName, callback) => {
        listeners.set(eventName, callback);
        return () => listeners.delete(eventName);
      },
    };
  }
  throw new Error(`Unexpected import: ${specifier}`);
};
const serviceModule = { exports: {} };
vm.runInNewContext(output, {
  exports: serviceModule.exports,
  module: serviceModule,
  require: mockRequire,
});

const { liveTranslationService } = serviceModule.exports;
const loaded = await liveTranslationService.getSettings();
assert.equal(loaded.has_api_key, true);
assert.equal(Object.hasOwn(loaded, 'api_key'), false);
const settingsInput = {
  enabled: true,
  source_language: 'ja',
  target_language: 'zh-CN',
  provider: 'openai',
  model: 'gpt-4o-mini',
  endpoint: 'https://api.openai.com/v1',
};
await liveTranslationService.saveSettings(settingsInput);
await liveTranslationService.setApiKey('sk-synthetic');
await liveTranslationService.clearApiKey();
await liveTranslationService.queue('source-event-1');

assert.deepEqual(
  calls.map(({ command }) => command),
  [
    'api_get_live_translation_settings',
    'api_save_live_translation_settings',
    'api_set_live_translation_api_key',
    'api_clear_live_translation_api_key',
    'api_queue_live_caption_translation',
  ],
);
assert.deepEqual(calls[1].args.settings, settingsInput);
assert.equal(Object.hasOwn(calls[1].args, 'input'), false);
assert.equal(calls[2].args.apiKey, 'sk-synthetic');
assert.equal(calls[4].args.eventId, 'source-event-1');
assert.equal(calls[0].args, undefined);
assert.equal(calls[3].args, undefined);

let observedSettings;
const unlisten = await liveTranslationService.onSettingsChanged((settings) => {
  observedSettings = settings;
});
listeners.get('live-translation-settings-changed')({ payload: publicSettings });
assert.equal(observedSettings.has_api_key, true);
unlisten();

console.log('live translation service tests passed');
