import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import { fileURLToPath } from 'node:url';

const testDirectory = path.dirname(fileURLToPath(import.meta.url));
const sourcePath = path.join(testDirectory, '..', '..', 'src', 'services', 'liveSummaryService.ts');
const output = ts.transpileModule(fs.readFileSync(sourcePath, 'utf8'), {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS,
    target: ts.ScriptTarget.ES2020,
    esModuleInterop: true,
  },
}).outputText;

function loadFreshService() {
  const calls = [];
  const serviceModule = { exports: {} };
  vm.runInNewContext(output, {
    exports: serviceModule.exports,
    module: serviceModule,
    require(specifier) {
      if (specifier === '@tauri-apps/api/core') {
        return {
          invoke(command, args) {
            calls.push({ command, args });
            if (command === 'api_live_summary_issue_binding_ticket') {
              return Promise.resolve({ scopeId: 'scope-issued', handle: 'handle-issued' });
            }
            return Promise.resolve([]);
          },
        };
      }
      if (specifier === '@tauri-apps/api/event') {
        return { listen: async () => () => {} };
      }
      throw new Error(`unexpected module: ${specifier}`);
    },
  });
  return { service: serviceModule.exports.liveSummaryService, calls };
}

const first = loadFreshService();
assert.equal(first.service.getRecordingBindingTicket(), null);

const issued = await first.service.issueRecordingBindingTicket('D:/synthetic/exact-folder');
assert.equal(issued.scopeId, 'scope-issued');
assert.equal(first.calls.length, 1);
assert.equal(first.calls[0].command, 'api_live_summary_issue_binding_ticket');
assert.equal(first.calls[0].args.meetingFolder, 'D:/synthetic/exact-folder');

first.service.rememberRecordingBindingTicket(issued);
const copy = first.service.getRecordingBindingTicket();
assert.equal(copy.handle, 'handle-issued');
copy.handle = 'renderer-mutated-copy';
assert.equal(
  first.service.getRecordingBindingTicket().handle,
  'handle-issued',
  'callers cannot mutate the process-memory capability by reference',
);

first.service.clearRecordingBindingTicket('another-handle');
assert.equal(first.service.getRecordingBindingTicket().handle, 'handle-issued');
first.service.clearRecordingBindingTicket('handle-issued');
assert.equal(first.service.getRecordingBindingTicket(), null);

first.service.rememberRecordingBindingTicket({ scopeId: 'scope-stale', handle: 'handle-stale' });
const restarted = loadFreshService();
assert.equal(
  restarted.service.getRecordingBindingTicket(),
  null,
  'a renderer restart must not revive a stale binding capability',
);

console.log('live summary service ticket lifecycle tests passed');
