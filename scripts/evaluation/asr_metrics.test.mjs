import assert from 'node:assert/strict';
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import {
  characterTokens,
  editStats,
  evaluateDocument,
  permutationCharacterStats,
} from './asr_metrics.mjs';

const scriptPath = join(dirname(fileURLToPath(import.meta.url)), 'asr_metrics.mjs');

test('Chinese CER normalizes width, case, punctuation and whitespace', () => {
  const stats = editStats(characterTokens('Ｍｅｅｔｉｌｙ，开始！'), characterTokens('meetily 开始'));
  assert.equal(stats.errors, 0);
  assert.equal(stats.referenceLength, 9);
});

test('edit statistics expose substitution, deletion and insertion counts', () => {
  const stats = editStats(['a', 'b', 'c'], ['a', 'x', 'c', 'd']);
  assert.deepEqual(stats, {
    errors: 2,
    substitutions: 1,
    deletions: 0,
    insertions: 1,
    referenceLength: 3,
  });
});

test('speaker-aware CER ignores anonymous speaker label permutation', () => {
  const reference = [
    { speaker: 'Alice', start_ms: 0, text: '确认接口' },
    { speaker: 'Bob', start_ms: 1000, text: '补充测试' },
  ];
  const hypothesis = [
    { speaker: 'S02', start_ms: 0, text: '确认接口' },
    { speaker: 'S01', start_ms: 1000, text: '补充测试' },
  ];
  const stats = permutationCharacterStats(reference, hypothesis);
  assert.equal(stats.errors, 0);
  assert.deepEqual(stats.mapping, [
    { referenceSpeaker: 'Alice', hypothesisSpeaker: 'S02' },
    { referenceSpeaker: 'Bob', hypothesisSpeaker: 'S01' },
  ]);
});

test('speaker-aware report exposes extra hypothesis speakers', () => {
  const stats = permutationCharacterStats(
    [{ speaker: 'A', start_ms: 0, text: '确认' }],
    [
      { speaker: 'S01', start_ms: 0, text: '确认' },
      { speaker: 'S02', start_ms: 1000, text: '幻觉' },
    ],
  );
  assert.deepEqual(stats.unmatchedHypothesisSpeakers, ['S02']);
  assert.ok(stats.errors > 0);
});

test('document report aggregates CER, WER, cpCER, RTF and latency percentiles', () => {
  const report = evaluateDocument({
    schema: 1,
    samples: [
      {
        id: 'zh-meeting-1',
        language: 'zh-CN',
        reference: [{ speaker: 'A', start_ms: 0, text: '开始会议' }],
        hypothesis: [{ speaker: 'S01', start_ms: 0, text: '开始会议' }],
        audio_duration_ms: 10_000,
        processing_duration_ms: 5_000,
        first_partial_latency_ms: 300,
        final_latency_ms: 900,
        peak_ram_mb: 512,
        peak_vram_mb: 1024,
      },
      {
        id: 'en-meeting-1',
        language: 'en',
        reference: 'ship the stable build',
        hypothesis: 'ship stable build',
        audio_duration_ms: 20_000,
        processing_duration_ms: 5_000,
        first_partial_latency_ms: 500,
        final_latency_ms: 1100,
      },
    ],
  });

  assert.equal(report.summary.sample_count, 2);
  assert.equal(report.summary.wer, 0.25);
  assert.deepEqual(report.summary.rtf, {
    weighted: 0.333333,
    p50: 0.375,
    p95: 0.4875,
    count: 2,
    missing: 0,
    estimator: 'linear_interpolation_r7',
  });
  assert.equal(report.summary.first_partial_latency_ms.p50, 400);
  assert.equal(report.summary.first_partial_latency_ms.p95, 490);
  assert.equal(report.summary.peak_vram_mb, 1024);
  assert.equal(report.samples[0].wer, null);
  assert.equal(report.samples[0].cp_cer, 0);
  assert.equal(report.summary.cp_cer_coverage, 0.5);
});

test('report preserves comparable run metadata and groups metrics by scenario', () => {
  const audioSha256 = 'a'.repeat(64);
  const report = evaluateDocument({
    schema: 1,
    run_metadata: {
      run_id: 'rtx4060-deepgram-001',
      engine: 'deepgram',
      model: 'nova-3',
      model_revision: '2026-08-15',
      device: 'RTX 4060 Laptop',
      configuration_fingerprint: 'sha256:config-v1',
      audio_manifest_revision: 'internal-consented-v2',
      partial_latency_origin: 'audio_enqueue_to_first_partial',
      final_latency_origin: 'vad_commit_to_final',
      clock: 'monotonic',
    },
    samples: [
      {
        id: 'clean-1',
        scenario: 'clean-single-speaker',
        audio_sha256: audioSha256,
        reference_revision: 'human-r3',
        language: 'zh-CN',
        reference: '开始会议',
        hypothesis: '开始会议',
        audio_duration_ms: 10_000,
        processing_duration_ms: 2_000,
      },
      {
        id: 'overlap-1',
        scenario: 'overlap-meeting',
        language: 'zh-CN',
        reference: '接口确认',
        hypothesis: '接口确认',
        audio_duration_ms: 10_000,
        processing_duration_ms: 6_000,
      },
    ],
  });

  assert.equal(report.run_metadata.model_revision, '2026-08-15');
  assert.equal(report.samples[0].audio_sha256, audioSha256);
  assert.equal(report.samples[0].reference_revision, 'human-r3');
  assert.equal(report.scenario_summaries['clean-single-speaker'].sample_count, 1);
  assert.equal(report.scenario_summaries['clean-single-speaker'].rtf.weighted, 0.2);
  assert.equal(report.scenario_summaries['overlap-meeting'].rtf.weighted, 0.6);
});

test('run metadata rejects unknown fields instead of silently weakening comparability', () => {
  assert.throws(
    () => evaluateDocument({
      schema: 1,
      run_metadata: { model: 'test', arbitrary_note: 'not part of schema 1' },
      samples: [{ id: 'metadata', language: 'zh', reference: '会议', hypothesis: '会议' }],
    }),
    /unsupported run_metadata field/,
  );
});

test('invalid duration pairs fail closed', () => {
  assert.throws(
    () => evaluateDocument({
      schema: 1,
      samples: [{ id: 'bad', reference: 'a', hypothesis: 'a', audio_duration_ms: 1000 }],
    }),
    /both audio and processing duration/,
  );
});

test('oversized alignment asks for shorter evaluation samples', () => {
  const longSequence = Array(7100).fill('字');
  assert.throws(
    () => editStats(longSequence, longSequence),
    /split the meeting into shorter evaluation samples/,
  );
});

test('unlabeled hypothesis is scored as one anonymous stream instead of disappearing', () => {
  const report = evaluateDocument({
    schema: 1,
    samples: [{
      id: 'unlabeled-system',
      language: 'zh',
      reference: [
        { speaker: 'A', start_ms: 0, text: '甲方发言' },
        { speaker: 'B', start_ms: 1000, text: '乙方发言' },
      ],
      hypothesis: '甲方发言乙方发言',
    }],
  });
  assert.equal(report.samples[0].cp_cer_eligible, true);
  assert.equal(report.samples[0].speaker_labels.hypothesis_complete, false);
  assert.ok(report.samples[0].cp_cer > 0);
  assert.equal(report.summary.cp_cer_coverage, 1);
});

test('incomplete reference speaker labels are reported as ineligible', () => {
  const report = evaluateDocument({
    schema: 1,
    samples: [{
      id: 'incomplete-reference',
      language: 'zh',
      reference: [
        { speaker: 'A', start_ms: 0, text: '已标注' },
        { start_ms: 1000, text: '未标注' },
      ],
      hypothesis: [{ speaker: 'S01', start_ms: 0, text: '已标注未标注' }],
    }],
  });
  assert.equal(report.samples[0].cp_cer, null);
  assert.equal(report.samples[0].cp_cer_eligible, false);
  assert.equal(report.summary.cp_cer_coverage, 0);
});

test('unknown language does not fabricate whitespace WER for CJK text', () => {
  const report = evaluateDocument({
    schema: 1,
    samples: [{ id: 'unknown-language', reference: '今天开始会议', hypothesis: '今天会议' }],
  });
  assert.equal(report.samples[0].wer, null);
  assert.equal(report.summary.wer_evaluated_samples, 0);
});

test('silence stress keeps hallucination counts even when an error rate has no denominator', () => {
  const report = evaluateDocument({
    schema: 1,
    samples: [{
      id: 'silence-hallucination',
      scenario: 'silence',
      language: 'zh',
      reference: '',
      hypothesis: '谢谢观看',
      audio_duration_ms: 60_000,
      processing_duration_ms: 1_000,
    }],
  });
  assert.equal(report.samples[0].cer, null);
  assert.equal(report.samples[0].cer_counts.insertions, 4);
  assert.equal(report.samples[0].silence_hallucinated_characters, 4);
  assert.deepEqual(report.summary.silence_stress, {
    reference_samples: 1,
    samples_with_duration: 1,
    hallucinated_characters: 4,
    hallucinated_characters_per_audio_minute: 4,
  });
  assert.equal(report.scenario_summaries.silence.silence_stress.hallucinated_characters, 4);
});

test('partially timed utterances are rejected because sorting would be ambiguous', () => {
  assert.throws(
    () => evaluateDocument({
      schema: 1,
      samples: [{
        id: 'partial-time',
        language: 'zh',
        reference: [
          { speaker: 'A', start_ms: 0, text: '第一句' },
          { speaker: 'A', text: '第二句' },
        ],
        hypothesis: '第一句第二句',
      }],
    }),
    /start_ms for every utterance or none/,
  );
});

test('CLI accepts UTF-8 BOM and Windows-safe paths with spaces and Chinese', () => {
  const temporaryRoot = mkdtempSync(join(tmpdir(), 'meetily-eval-'));
  const nestedDirectory = join(temporaryRoot, '中文 目录');
  const inputPath = join(nestedDirectory, '输入 样本.json');
  const outputPath = join(nestedDirectory, '输出 报告.json');
  mkdirSync(nestedDirectory);
  writeFileSync(inputPath, `\uFEFF${JSON.stringify({
    schema: 1,
    samples: [{ id: 'bom-cli', language: 'zh', reference: '会议', hypothesis: '会议' }],
  })}`, 'utf8');

  try {
    const result = spawnSync(process.execPath, [scriptPath, inputPath, '--output', outputPath], {
      encoding: 'utf8',
    });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(JSON.parse(readFileSync(outputPath, 'utf8')).summary.cer, 0);
  } finally {
    rmSync(temporaryRoot, { recursive: true, force: true });
  }
});
