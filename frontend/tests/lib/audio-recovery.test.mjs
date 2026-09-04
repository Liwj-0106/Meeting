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
  'audio-recovery.ts',
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

const { isAudioRecoverySafeToCleanup } = module.exports;

const verifiedTrack = {
  track: 'mixed',
  status: 'success',
  chunk_count: 2,
  audio_file_path: 'D:\\recordings\\meeting\\audio.mp4',
  verified: true,
  message: 'verified',
};

assert.equal(isAudioRecoverySafeToCleanup(null), false);
assert.equal(
  isAudioRecoverySafeToCleanup({
    status: 'success',
    chunk_count: 2,
    estimated_duration_seconds: 60,
    message: 'legacy response without proof',
  }),
  false,
  'legacy success without per-track proof must retain checkpoints',
);
assert.equal(
  isAudioRecoverySafeToCleanup({
    status: 'partial',
    chunk_count: 2,
    estimated_duration_seconds: 60,
    message: 'one raw track failed',
    cleanup_ready: false,
    tracks: [verifiedTrack],
  }),
  false,
);
assert.equal(
  isAudioRecoverySafeToCleanup({
    status: 'success',
    chunk_count: 2,
    estimated_duration_seconds: 60,
    message: 'backend verified',
    cleanup_ready: true,
    tracks: [verifiedTrack, { ...verifiedTrack, track: 'microphone', verified: false }],
  }),
  false,
  'one unverified raw track must block all cleanup',
);
assert.equal(
  isAudioRecoverySafeToCleanup({
    status: 'success',
    chunk_count: 4,
    estimated_duration_seconds: 120,
    message: 'backend verified',
    cleanup_ready: true,
    tracks: [verifiedTrack, { ...verifiedTrack, track: 'microphone' }],
  }),
  true,
);

console.log('audio recovery cleanup guard tests passed');
