import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const testDirectory = path.dirname(fileURLToPath(import.meta.url));
const frontendRoot = path.join(testDirectory, '..', '..');
const modulePath = path.join(frontendRoot, 'src', 'types', 'transcription-config.ts');
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
  configuredKeyForProvider,
  createDefaultTranscriptConfig,
  normalizeTranscriptConfig,
} = module.exports;

const defaults = createDefaultTranscriptConfig();
assert.equal(defaults.provider, 'parakeet');
assert.equal(defaults.streamingConfig.schemaVersion, 1);
assert.equal(defaults.streamingConfig.providers.deepgram.model, 'nova-3');
assert.equal(defaults.streamingConfig.providers.deepgram.diarization, true);
assert.equal(defaults.streamingConfig.providers.openai.diarization, false);
assert.equal(defaults.hasApiKey, false);

const normalized = normalizeTranscriptConfig({
  provider: 'deepgram',
  model: '',
  hasApiKey: true,
  streamingConfig: {
    schemaVersion: 1,
    providers: {
      deepgram: { language: 'zh', keywords: ['Meetily'] },
    },
  },
});
assert.equal(normalized.model, 'nova-3');
assert.equal(normalized.streamingConfig.providers.deepgram.language, 'zh');
assert.equal(normalized.streamingConfig.providers.deepgram.latencyMode, 'balanced');
assert.equal(normalized.streamingConfig.providers.openai.model, 'gpt-live-transcribe');
assert.equal(normalized.streamingConfig.providers.openai.diarization, false);
assert.equal(configuredKeyForProvider(normalized, 'deepgram'), true);
assert.equal(configuredKeyForProvider(normalized, 'openai'), false);

const publicInterface = source.match(/export interface TranscriptModelConfig\s*{([\s\S]*?)\n}/)?.[1];
assert.ok(publicInterface, 'public transcript configuration interface must exist');
assert.doesNotMatch(
  publicInterface,
  /\bapiKey\s*\??\s*:/,
  'the WebView configuration must not contain a plaintext API key field',
);

const sourceRoot = path.join(frontendRoot, 'src');
const sourceFiles = [];
const collectSourceFiles = (directory) => {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name);
    if (entry.isDirectory()) collectSourceFiles(entryPath);
    else if (/\.(?:ts|tsx)$/.test(entry.name)) sourceFiles.push(entryPath);
  }
};
collectSourceFiles(sourceRoot);
for (const filePath of sourceFiles) {
  const fileSource = fs.readFileSync(filePath, 'utf8');
  assert.doesNotMatch(
    fileSource,
    /api_get_transcript_api_key/,
    `plaintext transcript-key getter must not be callable from ${path.relative(frontendRoot, filePath)}`,
  );
}

const contextSource = fs.readFileSync(
  path.join(sourceRoot, 'contexts', 'ConfigContext.tsx'),
  'utf8',
);
assert.doesNotMatch(
  contextSource,
  /console\.(?:log|debug|info)\([^\n]*(?:transcriptModelConfig|transcript\s+config)/i,
  'ConfigContext must not log the transcript configuration object',
);

console.log('public transcription configuration tests passed');
