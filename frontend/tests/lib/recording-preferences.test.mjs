import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const testDirectory = path.dirname(fileURLToPath(import.meta.url));
const sourcePath = path.join(
  testDirectory,
  '..',
  '..',
  'src',
  'lib',
  'recording-preferences.ts',
);
const output = ts.transpileModule(fs.readFileSync(sourcePath, 'utf8'), {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS,
    target: ts.ScriptTarget.ES2020,
  },
}).outputText;
const sourceModule = { exports: {} };
vm.runInNewContext(output, {
  exports: sourceModule.exports,
  module: sourceModule,
  require,
  Promise,
  Error,
});

const {
  DISABLED_AUDIO_DEVICE,
  RecordingPreferencesSaveError,
  audioDeviceOptionValue,
  createRecordingPreferencesCoordinator,
  isSelectedAudioDeviceUnavailable,
  preferencesWithSelectedDevices,
  selectedDevicesFromPreferences,
} = sourceModule.exports;

const basePreferences = (overrides = {}) => ({
  save_folder: 'D:\\Meetily\\recordings',
  auto_save: true,
  file_format: 'mp4',
  preferred_mic_device: null,
  preferred_system_device: null,
  ...overrides,
});

const deferred = () => {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
};
const plain = (value) => JSON.parse(JSON.stringify(value));

assert.equal(DISABLED_AUDIO_DEVICE, 'disabled');
assert.deepEqual(
  plain(selectedDevicesFromPreferences(basePreferences({
    preferred_mic_device: 'disabled',
    preferred_system_device: 'Speakers (output)',
  }))),
  { micDevice: 'disabled', systemDevice: 'Speakers (output)' },
);
assert.deepEqual(
  plain(preferencesWithSelectedDevices(basePreferences({ auto_save: false }), {
    micDevice: 'USB microphone (input)',
    systemDevice: null,
  })),
  basePreferences({
    auto_save: false,
    preferred_mic_device: 'USB microphone (input)',
    preferred_system_device: null,
  }),
);

const devices = [
  { name: 'USB microphone', device_type: 'Input' },
  { name: 'Speakers', device_type: 'Output' },
];
assert.equal(audioDeviceOptionValue(devices[0]), 'USB microphone (input)');
assert.equal(isSelectedAudioDeviceUnavailable(null, devices, 'Input'), false);
assert.equal(isSelectedAudioDeviceUnavailable('disabled', devices, 'Input'), false);
assert.equal(
  isSelectedAudioDeviceUnavailable('USB microphone (input)', devices, 'Input'),
  false,
);
assert.equal(
  isSelectedAudioDeviceUnavailable('Removed microphone (input)', devices, 'Input'),
  true,
);

// A slow startup read must never overwrite a save requested while it is in flight.
{
  const slowLoad = deferred();
  const canonicalSave = basePreferences({ preferred_mic_device: 'disabled' });
  let getCalls = 0;
  const coordinator = createRecordingPreferencesCoordinator({
    getRecordingPreferences: () => {
      getCalls += 1;
      return slowLoad.promise;
    },
    saveRecordingPreferences: async () => canonicalSave,
  });

  const loadPromise = coordinator.load();
  const savePromise = coordinator.save(basePreferences({ preferred_mic_device: 'DISABLED' }));
  slowLoad.resolve(basePreferences({ preferred_mic_device: 'Old microphone (input)' }));

  const [loadResult, saveResult] = await Promise.all([loadPromise, savePromise]);
  assert.equal(getCalls, 1);
  assert.equal(loadResult.isLatest, false);
  assert.equal(saveResult.ok, true);
  assert.equal(saveResult.isLatest, true);
  assert.equal(saveResult.preferences.preferred_mic_device, 'disabled');
}

// Rapid saves are serialized on disk and only the latest completion may update context.
{
  const first = deferred();
  const second = deferred();
  const started = [];
  const coordinator = createRecordingPreferencesCoordinator({
    getRecordingPreferences: async () => basePreferences(),
    saveRecordingPreferences: async (preferences) => {
      started.push(preferences.preferred_mic_device);
      if (started.length === 1) return first.promise;
      return second.promise;
    },
  });

  const saveA = coordinator.save(basePreferences({ preferred_mic_device: 'Mic A (input)' }));
  const saveB = coordinator.save(basePreferences({ preferred_mic_device: 'Mic B (input)' }));
  await Promise.resolve();
  assert.deepEqual(started, ['Mic A (input)']);

  first.resolve(basePreferences({ preferred_mic_device: 'Mic A (input)' }));
  const resultA = await saveA;
  await Promise.resolve();
  assert.deepEqual(started, ['Mic A (input)', 'Mic B (input)']);
  assert.equal(resultA.ok, true);
  assert.equal(resultA.isLatest, false);

  second.resolve(basePreferences({ preferred_mic_device: 'Mic B (input)' }));
  const resultB = await saveB;
  assert.equal(resultB.ok, true);
  assert.equal(resultB.isLatest, true);
  assert.equal(resultB.preferences.preferred_mic_device, 'Mic B (input)');
}

// If A reaches disk and the newer B fails, A is the exact rollback target.
{
  let call = 0;
  let persisted = basePreferences();
  const canonicalA = basePreferences({
    preferred_mic_device: 'Mic A (input)',
    preferred_system_device: 'Speakers (output)',
  });
  const coordinator = createRecordingPreferencesCoordinator({
    getRecordingPreferences: async () => persisted,
    saveRecordingPreferences: async () => {
      call += 1;
      if (call === 1) {
        persisted = canonicalA;
        return canonicalA;
      }
      throw new Error('disk unavailable');
    },
  });

  const saveA = coordinator.save(basePreferences({ preferred_mic_device: 'Mic A (input)' }));
  const saveB = coordinator.save(basePreferences({ preferred_mic_device: 'Mic B (input)' }));
  const resultA = await saveA;
  const resultB = await saveB;
  assert.equal(resultA.ok, true);
  assert.equal(resultA.isLatest, false);
  assert.equal(resultB.ok, false);
  assert.equal(resultB.isLatest, true);
  assert.equal(resultB.preferences, canonicalA);

  const publicError = new RecordingPreferencesSaveError(resultB.error, resultB.preferences);
  assert.equal(publicError.recoveredPreferences, canonicalA);
  assert.match(publicError.message, /disk unavailable/);
}

// Device-only saves re-read inside the queue and preserve unrelated fields.
{
  let persisted = basePreferences({ auto_save: false, file_format: 'wav' });
  const coordinator = createRecordingPreferencesCoordinator({
    getRecordingPreferences: async () => persisted,
    saveRecordingPreferences: async (preferences) => {
      persisted = preferences;
      return preferences;
    },
  });
  const result = await coordinator.saveDevices({
    micDevice: 'disabled',
    systemDevice: null,
  });
  assert.equal(result.ok, true);
  assert.equal(persisted.auto_save, false);
  assert.equal(persisted.file_format, 'wav');
  assert.equal(persisted.preferred_mic_device, 'disabled');
}

// A first failed save can still recover the backend value before startup load finishes.
{
  const backendValue = basePreferences({
    preferred_mic_device: 'Stable microphone (input)',
  });
  const coordinator = createRecordingPreferencesCoordinator({
    getRecordingPreferences: async () => backendValue,
    saveRecordingPreferences: async () => {
      throw new Error('write rejected');
    },
  });
  const result = await coordinator.save(
    basePreferences({ preferred_mic_device: 'New microphone (input)' }),
  );
  assert.equal(result.ok, false);
  assert.equal(result.isLatest, true);
  assert.equal(result.preferences, backendValue);
}

console.log('recording preference coordinator tests passed');
