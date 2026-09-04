#!/usr/bin/env node

import { readFile, stat, writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

const MAX_SAMPLES = 10_000;
const MAX_TEXT_CHARS = 2_000_000;
const MAX_JOINED_TEXT_CHARS = 500_000;
const MAX_INPUT_BYTES = 64 * 1024 * 1024;
const MAX_UTTERANCES_PER_TRANSCRIPT = 10_000;
const MAX_TOTAL_UTTERANCES = 100_000;
const MAX_ALIGNMENT_TOKENS = 250_000;
const MAX_ALIGNMENT_CELLS = 10_000_000;
const MAX_TOTAL_ALIGNMENT_CELLS = 50_000_000;
const MAX_SPEAKERS_PER_SAMPLE = 64;
const UNLABELED_SPEAKER = '__meetily_unlabeled__';
const CJK_LANGUAGE_PREFIXES = ['zh', 'zho', 'cmn', 'yue', 'ja', 'jpn', 'ko', 'kor'];

function ensureFiniteNonNegative(value, field, required = false) {
  if (value === undefined || value === null) {
    if (required) throw new Error(`${field} is required`);
    return null;
  }
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) {
    throw new Error(`${field} must be a finite non-negative number`);
  }
  return value;
}

function normalizeLanguage(value) {
  return typeof value === 'string' ? value.trim().toLowerCase().replaceAll('_', '-') : '';
}

function optionalString(value, field, maxLength) {
  if (value === undefined || value === null) return null;
  if (typeof value !== 'string') throw new Error(`${field} must be a string`);
  const normalized = value.trim();
  if (!normalized || normalized.length > maxLength || /[\u0000-\u001f\u007f]/u.test(normalized)) {
    throw new Error(`${field} is invalid`);
  }
  return normalized;
}

function normalizeRunMetadata(value) {
  if (value === undefined || value === null) return null;
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('run_metadata must be an object');
  }
  const allowed = new Set([
    'run_id',
    'engine',
    'model',
    'model_revision',
    'device',
    'configuration_fingerprint',
    'audio_manifest_revision',
    'partial_latency_origin',
    'final_latency_origin',
    'clock',
  ]);
  const unknown = Object.keys(value).filter((field) => !allowed.has(field));
  if (unknown.length > 0) throw new Error(`unsupported run_metadata field: ${unknown[0]}`);
  const metadata = {
    run_id: optionalString(value.run_id, 'run_metadata.run_id', 128),
    engine: optionalString(value.engine, 'run_metadata.engine', 128),
    model: optionalString(value.model, 'run_metadata.model', 256),
    model_revision: optionalString(value.model_revision, 'run_metadata.model_revision', 256),
    device: optionalString(value.device, 'run_metadata.device', 256),
    configuration_fingerprint: optionalString(
      value.configuration_fingerprint,
      'run_metadata.configuration_fingerprint',
      256,
    ),
    audio_manifest_revision: optionalString(
      value.audio_manifest_revision,
      'run_metadata.audio_manifest_revision',
      256,
    ),
    partial_latency_origin: optionalString(
      value.partial_latency_origin,
      'run_metadata.partial_latency_origin',
      128,
    ),
    final_latency_origin: optionalString(
      value.final_latency_origin,
      'run_metadata.final_latency_origin',
      128,
    ),
    clock: optionalString(value.clock, 'run_metadata.clock', 64),
  };
  return Object.fromEntries(Object.entries(metadata).filter(([, fieldValue]) => fieldValue !== null));
}

export function normalizeText(value) {
  if (typeof value !== 'string') throw new Error('transcript text must be a string');
  return value.normalize('NFKC').toLocaleLowerCase('und').replace(/\s+/gu, ' ').trim();
}

export function characterTokens(value) {
  return [...normalizeText(value)]
    .filter((character) => !/\s/gu.test(character))
    .filter((character) => !/[\p{P}\p{S}]/gu.test(character));
}

export function wordTokens(value) {
  const normalized = normalizeText(value)
    .replace(/[\p{P}\p{S}]+/gu, ' ')
    .replace(/\s+/gu, ' ')
    .trim();
  return normalized ? normalized.split(' ') : [];
}

export function editStats(reference, hypothesis, budget = null) {
  if (reference.length > MAX_ALIGNMENT_TOKENS || hypothesis.length > MAX_ALIGNMENT_TOKENS) {
    throw new Error('one transcript is too long; split the meeting into shorter evaluation samples');
  }
  const alignmentCells = reference.length * hypothesis.length;
  if (alignmentCells > MAX_ALIGNMENT_CELLS) {
    throw new Error('one alignment is too large; split the meeting into shorter evaluation samples');
  }
  if (budget) {
    if (alignmentCells > budget.remaining) {
      throw new Error('combined alignment budget exceeded; split this evaluation into smaller runs');
    }
    budget.remaining -= alignmentCells;
  }

  if (reference.length === 0) {
    return {
      errors: hypothesis.length,
      substitutions: 0,
      deletions: 0,
      insertions: hypothesis.length,
      referenceLength: 0,
    };
  }
  if (hypothesis.length === 0) {
    return {
      errors: reference.length,
      substitutions: 0,
      deletions: reference.length,
      insertions: 0,
      referenceLength: reference.length,
    };
  }

  const columns = hypothesis.length + 1;
  let previousErrors = new Uint32Array(columns);
  let previousSubstitutions = new Uint32Array(columns);
  let previousDeletions = new Uint32Array(columns);
  let previousInsertions = new Uint32Array(columns);
  let currentErrors = new Uint32Array(columns);
  let currentSubstitutions = new Uint32Array(columns);
  let currentDeletions = new Uint32Array(columns);
  let currentInsertions = new Uint32Array(columns);

  for (let column = 1; column < columns; column += 1) {
    previousErrors[column] = column;
    previousInsertions[column] = column;
  }

  for (let row = 1; row <= reference.length; row += 1) {
    currentErrors[0] = row;
    currentSubstitutions[0] = 0;
    currentDeletions[0] = row;
    currentInsertions[0] = 0;

    for (let column = 1; column < columns; column += 1) {
      const same = reference[row - 1] === hypothesis[column - 1];
      const diagonalErrors = previousErrors[column - 1] + (same ? 0 : 1);
      const deletionErrors = previousErrors[column] + 1;
      const insertionErrors = currentErrors[column - 1] + 1;

      // Stable tie-breaking: exact/substitution, then deletion, then insertion.
      // This keeps S/D/I diagnostics reproducible while preserving minimum distance.
      if (diagonalErrors <= deletionErrors && diagonalErrors <= insertionErrors) {
        currentErrors[column] = diagonalErrors;
        currentSubstitutions[column] = previousSubstitutions[column - 1] + (same ? 0 : 1);
        currentDeletions[column] = previousDeletions[column - 1];
        currentInsertions[column] = previousInsertions[column - 1];
      } else if (deletionErrors <= insertionErrors) {
        currentErrors[column] = deletionErrors;
        currentSubstitutions[column] = previousSubstitutions[column];
        currentDeletions[column] = previousDeletions[column] + 1;
        currentInsertions[column] = previousInsertions[column];
      } else {
        currentErrors[column] = insertionErrors;
        currentSubstitutions[column] = currentSubstitutions[column - 1];
        currentDeletions[column] = currentDeletions[column - 1];
        currentInsertions[column] = currentInsertions[column - 1] + 1;
      }
    }

    [previousErrors, currentErrors] = [currentErrors, previousErrors];
    [previousSubstitutions, currentSubstitutions] = [currentSubstitutions, previousSubstitutions];
    [previousDeletions, currentDeletions] = [currentDeletions, previousDeletions];
    [previousInsertions, currentInsertions] = [currentInsertions, previousInsertions];
  }

  const last = hypothesis.length;
  return {
    errors: previousErrors[last],
    substitutions: previousSubstitutions[last],
    deletions: previousDeletions[last],
    insertions: previousInsertions[last],
    referenceLength: reference.length,
  };
}

function mergeStats(target, source) {
  target.errors += source.errors;
  target.substitutions += source.substitutions;
  target.deletions += source.deletions;
  target.insertions += source.insertions;
  target.referenceLength += source.referenceLength;
}

function rate(stats) {
  if (stats.referenceLength === 0) return stats.errors === 0 ? 0 : null;
  return stats.errors / stats.referenceLength;
}

function transcriptItems(value, field) {
  if (typeof value === 'string') return [{ text: value, speaker: null, start_ms: null }];
  if (!Array.isArray(value)) throw new Error(`${field} must be a string or an array`);
  if (value.length > MAX_UTTERANCES_PER_TRANSCRIPT) {
    throw new Error(`${field} has too many utterances`);
  }
  const items = value.map((item, index) => {
    if (!item || typeof item !== 'object' || Array.isArray(item)) {
      throw new Error(`${field}[${index}] must be an object`);
    }
    if (typeof item.text !== 'string') throw new Error(`${field}[${index}].text must be a string`);
    const speaker = item.speaker === undefined || item.speaker === null
      ? null
      : String(item.speaker).trim();
    if (speaker !== null && !speaker) throw new Error(`${field}[${index}].speaker is empty`);
    if (speaker && (speaker.length > 256 || speaker === UNLABELED_SPEAKER)) {
      throw new Error(`${field}[${index}].speaker is invalid`);
    }
    const start = ensureFiniteNonNegative(item.start_ms, `${field}[${index}].start_ms`);
    return { text: item.text, speaker, start_ms: start, originalIndex: index };
  });
  const timedItems = items.filter((item) => item.start_ms !== null).length;
  if (timedItems !== 0 && timedItems !== items.length) {
    throw new Error(`${field} must provide start_ms for every utterance or none of them`);
  }
  return items;
}

function joinedText(items) {
  return items.map((item) => item.text).join(' ');
}

function speakerStreams(items, allowUnlabeled) {
  if (!allowUnlabeled && items.some((item) => !item.speaker)) return null;
  const streams = new Map();
  const sorted = [...items].sort((left, right) =>
    (left.start_ms ?? Number.MAX_SAFE_INTEGER) - (right.start_ms ?? Number.MAX_SAFE_INTEGER)
      || left.originalIndex - right.originalIndex);
  for (const item of sorted) {
    const speaker = item.speaker ?? UNLABELED_SPEAKER;
    const existing = streams.get(speaker) ?? [];
    existing.push(item.text);
    streams.set(speaker, existing);
  }
  if (streams.size > MAX_SPEAKERS_PER_SAMPLE) throw new Error('too many speakers in one sample');
  return [...streams.entries()]
    .sort(([left], [right]) => (left === right ? 0 : left < right ? -1 : 1))
    .map(([speaker, texts]) => ({
      speaker: speaker === UNLABELED_SPEAKER ? null : speaker,
      tokens: characterTokens(texts.join(' ')),
    }));
}

// Minimum-cost assignment for a square integer matrix (Hungarian algorithm).
function minimumAssignment(costs) {
  const size = costs.length;
  if (size === 0) return { cost: 0, assignment: [] };
  if (costs.some((row) => row.length !== size)) throw new Error('assignment matrix must be square');

  const u = Array(size + 1).fill(0);
  const v = Array(size + 1).fill(0);
  const p = Array(size + 1).fill(0);
  const way = Array(size + 1).fill(0);

  for (let row = 1; row <= size; row += 1) {
    p[0] = row;
    let column0 = 0;
    const minValue = Array(size + 1).fill(Number.POSITIVE_INFINITY);
    const used = Array(size + 1).fill(false);
    do {
      used[column0] = true;
      const row0 = p[column0];
      let delta = Number.POSITIVE_INFINITY;
      let column1 = 0;
      for (let column = 1; column <= size; column += 1) {
        if (used[column]) continue;
        const current = costs[row0 - 1][column - 1] - u[row0] - v[column];
        if (current < minValue[column]) {
          minValue[column] = current;
          way[column] = column0;
        }
        if (minValue[column] < delta) {
          delta = minValue[column];
          column1 = column;
        }
      }
      for (let column = 0; column <= size; column += 1) {
        if (used[column]) {
          u[p[column]] += delta;
          v[column] -= delta;
        } else {
          minValue[column] -= delta;
        }
      }
      column0 = column1;
    } while (p[column0] !== 0);

    do {
      const column1 = way[column0];
      p[column0] = p[column1];
      column0 = column1;
    } while (column0 !== 0);
  }

  const assignment = Array(size).fill(-1);
  for (let column = 1; column <= size; column += 1) {
    assignment[p[column] - 1] = column - 1;
  }
  return {
    cost: assignment.reduce((total, column, row) => total + costs[row][column], 0),
    assignment,
  };
}

export function permutationCharacterStats(referenceItems, hypothesisItems, budget = null) {
  const reference = speakerStreams(referenceItems, false);
  const hypothesis = speakerStreams(hypothesisItems, true);
  if (!reference || reference.length === 0) return null;
  const size = Math.max(reference.length, hypothesis.length);
  const costs = Array.from({ length: size }, (_, row) =>
    Array.from({ length: size }, (_, column) => {
      const referenceTokens = reference[row]?.tokens ?? [];
      const hypothesisTokens = hypothesis[column]?.tokens ?? [];
      return editStats(referenceTokens, hypothesisTokens, budget).errors;
    }));
  const { cost, assignment } = minimumAssignment(costs);
  return {
    errors: cost,
    referenceLength: reference.reduce((total, stream) => total + stream.tokens.length, 0),
    mapping: assignment
      .slice(0, reference.length)
      .map((column, row) => ({
        referenceSpeaker: reference[row].speaker,
        hypothesisSpeaker: hypothesis[column]?.speaker ?? null,
      })),
    unmatchedHypothesisSpeakers: assignment
      .slice(reference.length)
      .map((column) => hypothesis[column]?.speaker ?? null)
      .filter((speaker, index, values) =>
        columnIsRealHypothesis(assignment[reference.length + index], hypothesis.length)
          && values.indexOf(speaker) === index),
  };
}

function columnIsRealHypothesis(column, hypothesisLength) {
  return Number.isInteger(column) && column >= 0 && column < hypothesisLength;
}

function percentile(values, fraction) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((left, right) => left - right);
  const index = (sorted.length - 1) * fraction;
  const lower = Math.floor(index);
  const upper = Math.ceil(index);
  if (lower === upper) return sorted[lower];
  return sorted[lower] + (sorted[upper] - sorted[lower]) * (index - lower);
}

function rounded(value) {
  return value === null ? null : Number(value.toFixed(6));
}

function distribution(values, totalSamples) {
  return {
    p50: rounded(percentile(values, 0.5)),
    p95: rounded(percentile(values, 0.95)),
    count: values.length,
    missing: totalSamples - values.length,
    estimator: 'linear_interpolation_r7',
  };
}

function summarizeScenario(samples) {
  const cer = { errors: 0, substitutions: 0, deletions: 0, insertions: 0, referenceLength: 0 };
  const wer = { errors: 0, substitutions: 0, deletions: 0, insertions: 0, referenceLength: 0 };
  let werSamples = 0;
  let cpErrors = 0;
  let cpReferenceLength = 0;
  let cpSamples = 0;
  let audioMs = 0;
  let processingMs = 0;
  const rtfs = [];
  const partialLatencies = [];
  const finalLatencies = [];
  const ram = [];
  const vram = [];
  let silenceReferenceSamples = 0;
  let silenceSamplesWithDuration = 0;
  let silenceAudioMs = 0;
  let silenceHallucinatedCharacters = 0;

  for (const sample of samples) {
    mergeStats(cer, sample.cer_counts);
    if (sample.wer_counts) {
      mergeStats(wer, sample.wer_counts);
      werSamples += 1;
    }
    if (sample.cp_cer_counts) {
      cpErrors += sample.cp_cer_counts.errors;
      cpReferenceLength += sample.cp_cer_counts.referenceLength;
      cpSamples += 1;
    }
    if (sample.rtf !== null) rtfs.push(sample.rtf);
    if (sample.audio_duration_ms !== null) {
      audioMs += sample.audio_duration_ms;
      processingMs += sample.processing_duration_ms;
    }
    if (sample.first_partial_latency_ms !== null) partialLatencies.push(sample.first_partial_latency_ms);
    if (sample.final_latency_ms !== null) finalLatencies.push(sample.final_latency_ms);
    if (sample.peak_ram_mb !== null) ram.push(sample.peak_ram_mb);
    if (sample.peak_vram_mb !== null) vram.push(sample.peak_vram_mb);
    if (sample.silence_reference) {
      silenceReferenceSamples += 1;
      silenceHallucinatedCharacters += sample.silence_hallucinated_characters;
      if (sample.audio_duration_ms !== null) {
        silenceSamplesWithDuration += 1;
        silenceAudioMs += sample.audio_duration_ms;
      }
    }
  }

  return {
    sample_count: samples.length,
    cer: rounded(rate(cer)),
    wer: werSamples > 0 ? rounded(rate(wer)) : null,
    cp_cer: cpReferenceLength > 0 ? rounded(cpErrors / cpReferenceLength) : null,
    cp_cer_coverage: rounded(cpSamples / samples.length),
    rtf: {
      weighted: audioMs > 0 ? rounded(processingMs / audioMs) : null,
      ...distribution(rtfs, samples.length),
    },
    first_partial_latency_ms: distribution(partialLatencies, samples.length),
    final_latency_ms: distribution(finalLatencies, samples.length),
    peak_ram_mb: ram.length > 0 ? Math.max(...ram) : null,
    peak_vram_mb: vram.length > 0 ? Math.max(...vram) : null,
    silence_stress: {
      reference_samples: silenceReferenceSamples,
      samples_with_duration: silenceSamplesWithDuration,
      hallucinated_characters: silenceHallucinatedCharacters,
      hallucinated_characters_per_audio_minute: silenceAudioMs > 0
        ? rounded(silenceHallucinatedCharacters / (silenceAudioMs / 60_000))
        : null,
    },
  };
}

export function evaluateDocument(document) {
  if (!document || typeof document !== 'object' || Array.isArray(document)) {
    throw new Error('evaluation input must be an object');
  }
  if (document.schema !== 1) throw new Error('unsupported evaluation schema');
  if (!Array.isArray(document.samples) || document.samples.length === 0) {
    throw new Error('samples must be a non-empty array');
  }
  if (document.samples.length > MAX_SAMPLES) throw new Error('too many samples');
  const runMetadata = normalizeRunMetadata(document.run_metadata);

  const corpusCer = { errors: 0, substitutions: 0, deletions: 0, insertions: 0, referenceLength: 0 };
  const corpusWer = { errors: 0, substitutions: 0, deletions: 0, insertions: 0, referenceLength: 0 };
  let cpErrors = 0;
  let cpReferenceLength = 0;
  let totalCharacters = 0;
  let totalUtterances = 0;
  let totalAudioMs = 0;
  let totalProcessingMs = 0;
  const firstPartialLatencies = [];
  const finalLatencies = [];
  const realTimeFactors = [];
  const peakRamValues = [];
  const peakVramValues = [];
  const alignmentBudget = { remaining: MAX_TOTAL_ALIGNMENT_CELLS };
  let cpCerEligibleSamples = 0;
  let werEvaluatedSamples = 0;
  let silenceReferenceSamples = 0;
  let silenceSamplesWithDuration = 0;
  let silenceAudioMs = 0;
  let silenceHallucinatedCharacters = 0;

  const samples = document.samples.map((sample, sampleIndex) => {
    if (!sample || typeof sample !== 'object' || Array.isArray(sample)) {
      throw new Error(`samples[${sampleIndex}] must be an object`);
    }
    const id = String(sample.id ?? '').trim();
    if (!id) throw new Error(`samples[${sampleIndex}].id is required`);
    if (id.length > 512) throw new Error(`samples[${sampleIndex}].id is too long`);
    const language = normalizeLanguage(sample.language);
    if (language.length > 64) throw new Error(`samples[${sampleIndex}].language is too long`);
    const scenario = optionalString(sample.scenario, `samples[${sampleIndex}].scenario`, 128)
      ?? 'unclassified';
    const audioSha256 = optionalString(sample.audio_sha256, `samples[${sampleIndex}].audio_sha256`, 64);
    if (audioSha256 && !/^[a-f0-9]{64}$/iu.test(audioSha256)) {
      throw new Error(`samples[${sampleIndex}].audio_sha256 must contain 64 hexadecimal characters`);
    }
    const referenceRevision = optionalString(
      sample.reference_revision,
      `samples[${sampleIndex}].reference_revision`,
      128,
    );
    const referenceItems = transcriptItems(sample.reference, `samples[${sampleIndex}].reference`);
    const hypothesisItems = transcriptItems(sample.hypothesis, `samples[${sampleIndex}].hypothesis`);
    totalUtterances += referenceItems.length + hypothesisItems.length;
    if (totalUtterances > MAX_TOTAL_UTTERANCES) throw new Error('combined utterance count is too large');
    totalCharacters += referenceItems.reduce((total, item) => total + item.text.length, 0)
      + hypothesisItems.reduce((total, item) => total + item.text.length, 0);
    if (totalCharacters > MAX_TEXT_CHARS) throw new Error('combined transcript text is too large');

    const referenceText = joinedText(referenceItems);
    const hypothesisText = joinedText(hypothesisItems);
    if (referenceText.length > MAX_JOINED_TEXT_CHARS || hypothesisText.length > MAX_JOINED_TEXT_CHARS) {
      throw new Error(`samples[${sampleIndex}] transcript is too long; split it into shorter samples`);
    }
    const referenceCharacterTokens = characterTokens(referenceText);
    const hypothesisCharacterTokens = characterTokens(hypothesisText);
    const cerStats = editStats(referenceCharacterTokens, hypothesisCharacterTokens, alignmentBudget);
    mergeStats(corpusCer, cerStats);
    const silenceReference = referenceCharacterTokens.length === 0;
    const sampleHallucinatedCharacters = silenceReference ? hypothesisCharacterTokens.length : 0;
    if (silenceReference) {
      silenceReferenceSamples += 1;
      silenceHallucinatedCharacters += sampleHallucinatedCharacters;
    }

    const isCjk = CJK_LANGUAGE_PREFIXES.some((prefix) =>
      language === prefix || language.startsWith(`${prefix}-`));
    const werStats = !language || isCjk
      ? null
      : editStats(wordTokens(referenceText), wordTokens(hypothesisText), alignmentBudget);
    if (werStats) {
      mergeStats(corpusWer, werStats);
      werEvaluatedSamples += 1;
    }

    const cpStats = permutationCharacterStats(referenceItems, hypothesisItems, alignmentBudget);
    if (cpStats) {
      cpErrors += cpStats.errors;
      cpReferenceLength += cpStats.referenceLength;
      cpCerEligibleSamples += 1;
    }

    const audioDurationMs = ensureFiniteNonNegative(
      sample.audio_duration_ms,
      `samples[${sampleIndex}].audio_duration_ms`,
    );
    const processingDurationMs = ensureFiniteNonNegative(
      sample.processing_duration_ms,
      `samples[${sampleIndex}].processing_duration_ms`,
    );
    if ((audioDurationMs === null) !== (processingDurationMs === null)) {
      throw new Error(`samples[${sampleIndex}] must provide both audio and processing duration`);
    }
    if (audioDurationMs !== null && audioDurationMs <= 0) {
      throw new Error(`samples[${sampleIndex}].audio_duration_ms must be greater than zero`);
    }
    if (audioDurationMs !== null) {
      totalAudioMs += audioDurationMs;
      totalProcessingMs += processingDurationMs;
      realTimeFactors.push(processingDurationMs / audioDurationMs);
      if (silenceReference) {
        silenceSamplesWithDuration += 1;
        silenceAudioMs += audioDurationMs;
      }
    }

    const firstPartialLatency = ensureFiniteNonNegative(
      sample.first_partial_latency_ms,
      `samples[${sampleIndex}].first_partial_latency_ms`,
    );
    const finalLatency = ensureFiniteNonNegative(
      sample.final_latency_ms,
      `samples[${sampleIndex}].final_latency_ms`,
    );
    const peakRam = ensureFiniteNonNegative(sample.peak_ram_mb, `samples[${sampleIndex}].peak_ram_mb`);
    const peakVram = ensureFiniteNonNegative(sample.peak_vram_mb, `samples[${sampleIndex}].peak_vram_mb`);
    if (firstPartialLatency !== null) firstPartialLatencies.push(firstPartialLatency);
    if (finalLatency !== null) finalLatencies.push(finalLatency);
    if (peakRam !== null) peakRamValues.push(peakRam);
    if (peakVram !== null) peakVramValues.push(peakVram);

    return {
      id,
      scenario,
      audio_sha256: audioSha256,
      reference_revision: referenceRevision,
      language: language || null,
      cer: rounded(rate(cerStats)),
      cer_counts: cerStats,
      wer: werStats ? rounded(rate(werStats)) : null,
      wer_counts: werStats,
      cp_cer: cpStats ? rounded(cpStats.referenceLength === 0 ? null : cpStats.errors / cpStats.referenceLength) : null,
      cp_cer_counts: cpStats
        ? { errors: cpStats.errors, referenceLength: cpStats.referenceLength }
        : null,
      cp_cer_eligible: cpStats !== null,
      speaker_labels: {
        reference_complete: referenceItems.length > 0 && referenceItems.every((item) => item.speaker),
        hypothesis_complete: hypothesisItems.length > 0 && hypothesisItems.every((item) => item.speaker),
      },
      speaker_mapping: cpStats?.mapping ?? null,
      unmatched_hypothesis_speakers: cpStats?.unmatchedHypothesisSpeakers ?? null,
      rtf: audioDurationMs === null ? null : rounded(processingDurationMs / audioDurationMs),
      audio_duration_ms: audioDurationMs,
      processing_duration_ms: processingDurationMs,
      first_partial_latency_ms: firstPartialLatency,
      final_latency_ms: finalLatency,
      peak_ram_mb: peakRam,
      peak_vram_mb: peakVram,
      silence_reference: silenceReference,
      silence_hallucinated_characters: sampleHallucinatedCharacters,
    };
  });

  const samplesByScenario = new Map();
  for (const sample of samples) {
    const group = samplesByScenario.get(sample.scenario) ?? [];
    group.push(sample);
    samplesByScenario.set(sample.scenario, group);
  }
  const scenarioSummaries = Object.fromEntries(
    [...samplesByScenario.entries()]
      .sort(([left], [right]) => (left === right ? 0 : left < right ? -1 : 1))
      .map(([scenario, scenarioSamples]) => [scenario, summarizeScenario(scenarioSamples)]),
  );

  return {
    schema: 1,
    run_metadata: runMetadata,
    normalization: {
      unicode: 'NFKC',
      case: 'lowercase',
      cer_ignores: ['whitespace', 'Unicode punctuation', 'Unicode symbols'],
      wer_ignores: ['Unicode punctuation', 'Unicode symbols'],
      cjk_wer: 'not reported without an external word segmenter',
      unknown_language_wer: 'not reported',
      percentile_estimator: 'linear_interpolation_r7',
    },
    summary: {
      sample_count: samples.length,
      cer: rounded(rate(corpusCer)),
      cer_counts: corpusCer,
      wer: werEvaluatedSamples > 0 ? rounded(rate(corpusWer)) : null,
      wer_counts: werEvaluatedSamples > 0 ? corpusWer : null,
      wer_evaluated_samples: werEvaluatedSamples,
      cp_cer: cpReferenceLength > 0 ? rounded(cpErrors / cpReferenceLength) : null,
      cp_cer_counts: cpReferenceLength > 0
        ? { errors: cpErrors, referenceLength: cpReferenceLength }
        : null,
      cp_cer_eligible_samples: cpCerEligibleSamples,
      cp_cer_coverage: rounded(cpCerEligibleSamples / samples.length),
      rtf: {
        weighted: totalAudioMs > 0 ? rounded(totalProcessingMs / totalAudioMs) : null,
        ...distribution(realTimeFactors, samples.length),
      },
      first_partial_latency_ms: distribution(firstPartialLatencies, samples.length),
      final_latency_ms: distribution(finalLatencies, samples.length),
      peak_ram_mb: peakRamValues.length > 0 ? Math.max(...peakRamValues) : null,
      peak_vram_mb: peakVramValues.length > 0 ? Math.max(...peakVramValues) : null,
      silence_stress: {
        reference_samples: silenceReferenceSamples,
        samples_with_duration: silenceSamplesWithDuration,
        hallucinated_characters: silenceHallucinatedCharacters,
        hallucinated_characters_per_audio_minute: silenceAudioMs > 0
          ? rounded(silenceHallucinatedCharacters / (silenceAudioMs / 60_000))
          : null,
      },
    },
    scenario_summaries: scenarioSummaries,
    samples,
  };
}

function parseArguments(argv) {
  const argumentsCopy = [...argv];
  const input = argumentsCopy.shift();
  if (!input) throw new Error('usage: node scripts/evaluation/asr_metrics.mjs <input.json> [--output <report.json>]');
  let output = null;
  while (argumentsCopy.length > 0) {
    const option = argumentsCopy.shift();
    if (option === '--output' && argumentsCopy.length > 0) {
      output = argumentsCopy.shift();
    } else {
      throw new Error(`unsupported argument: ${option}`);
    }
  }
  return { input, output };
}

async function main() {
  const { input, output } = parseArguments(process.argv.slice(2));
  const inputMetadata = await stat(input);
  if (!inputMetadata.isFile()) throw new Error('evaluation input must be a file');
  if (inputMetadata.size > MAX_INPUT_BYTES) {
    throw new Error('evaluation input is too large; split it into smaller runs');
  }
  const inputText = await readFile(input, 'utf8');
  const source = JSON.parse(inputText.startsWith('\uFEFF') ? inputText.slice(1) : inputText);
  const report = `${JSON.stringify(evaluateDocument(source), null, 2)}\n`;
  if (output) {
    await writeFile(output, report, 'utf8');
  } else {
    process.stdout.write(report);
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  main().catch((error) => {
    process.stderr.write(`ASR evaluation failed: ${error.message}\n`);
    process.exitCode = 1;
  });
}
