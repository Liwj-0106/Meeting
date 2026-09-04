import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import { fileURLToPath } from 'node:url';

const testDirectory = path.dirname(fileURLToPath(import.meta.url));
const frontendRoot = path.join(testDirectory, '..', '..');
const readSource = (...segments) => fs.readFileSync(path.join(frontendRoot, ...segments), 'utf8');

const serviceOutput = ts.transpileModule(readSource('src', 'services', 'configService.ts'), {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS,
    target: ts.ScriptTarget.ES2020,
  },
}).outputText;
const calls = [];
const canonical = {
  save_folder: 'D:\\Meetily\\recordings',
  auto_save: true,
  file_format: 'mp4',
  preferred_mic_device: 'disabled',
  preferred_system_device: null,
};
const serviceModule = { exports: {} };
vm.runInNewContext(serviceOutput, {
  exports: serviceModule.exports,
  module: serviceModule,
  require: (specifier) => {
    if (specifier === '@tauri-apps/api/core') {
      return {
        invoke: async (command, args) => {
          calls.push({ command, args });
          return canonical;
        },
      };
    }
    throw new Error(`Unexpected runtime import: ${specifier}`);
  },
});

const service = new serviceModule.exports.ConfigService();
const returned = await service.saveRecordingPreferences({
  ...canonical,
  preferred_mic_device: ' DISABLED ',
});
assert.equal(returned, canonical);
assert.equal(calls.length, 1);
assert.equal(calls[0].command, 'set_recording_preferences');
assert.equal(calls[0].args.preferences.preferred_mic_device, ' DISABLED ');

const recordingSettings = readSource('src', 'components', 'RecordingSettings.tsx');
assert.match(recordingSettings, /const saved = await saveRecordingPreferences\(prefs\)/);
assert.match(recordingSettings, /committedPreferencesRef\.current = saved/);
assert.match(recordingSettings, /onSave\?\.\(saved\)/);
assert.match(recordingSettings, /error\.recoveredPreferences/);
assert.doesNotMatch(recordingSettings, /invoke\(['"]set_recording_preferences/);

const context = readSource('src', 'contexts', 'ConfigContext.tsx');
assert.match(context, /createRecordingPreferencesCoordinator\(configService\)/);
assert.match(context, /if \(active && result\.isLatest\)/);
assert.match(context, /if \(result\.isLatest && result\.preferences\)/);
assert.match(context, /saveSelectedDevices/);

const modal = readSource('src', 'app', '_components', 'SettingsModal.tsx');
assert.match(modal, /selectedDevices=\{deviceDraft\}/);
assert.match(modal, /onDeviceChange=\{setDeviceDraft\}/);
assert.match(modal, /await saveSelectedDevices\(deviceDraft\)/);
assert.match(modal, /保存并应用/);
assert.doesNotMatch(modal, /onDeviceChange=\{setSelectedDevices\}/);

const deviceSelection = readSource('src', 'components', 'DeviceSelection.tsx');
assert.match(deviceSelection, /isSelectedAudioDeviceUnavailable/);
assert.match(deviceSelection, /不使用麦克风（仅系统音频）/);
assert.match(deviceSelection, /当前不可用/);
assert.match(deviceSelection, /刷新不会清除选择/);

console.log('recording device settings integration tests passed');

