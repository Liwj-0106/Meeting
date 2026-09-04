import type {
  Transcript,
  TranscriptDiarizationMetadata,
  TranscriptEventKind,
  TranscriptSpeaker,
  TranscriptUpdate,
} from '../types';

/** A transcript update after legacy and v1 fields have been reconciled. */
export interface NormalizedTranscriptUpdate extends TranscriptUpdate {
  schema_version: number;
  revision: number;
  event_kind: TranscriptEventKind;
  is_stable: boolean;
  start_ms: number;
  end_ms: number;
}

export interface TranscriptConversionOptions {
  /**
   * Stable ID supplied by the caller for a legacy event.
   *
   * Revisable events use `utterance_id`; legacy events intentionally do not
   * deduplicate, so a reducer normally supplies an append-position-based ID.
   */
  legacyId?: string;
}

const finiteNumber = (value: number | undefined): number | undefined =>
  typeof value === 'number' && Number.isFinite(value) ? value : undefined;

const nonNegativeInteger = (value: number | undefined, fallback: number): number => {
  const normalized = finiteNumber(value);
  return normalized === undefined ? fallback : Math.max(0, Math.trunc(normalized));
};

const nonNegativeRoundedInteger = (value: number | undefined, fallback: number): number => {
  const normalized = finiteNumber(value);
  return normalized === undefined ? fallback : Math.max(0, Math.round(normalized));
};

const nonEmptyString = (value: string | undefined): string | undefined => {
  const normalized = value?.trim();
  return normalized ? normalized : undefined;
};

const normalizeSpeaker = (
  speaker: TranscriptSpeaker | undefined,
  flatSpeakerId: string | undefined,
  flatLocalLabel: string | undefined,
  flatDisplayName: string | undefined,
  flatConfidence: number | undefined,
  flatStatus: TranscriptSpeaker['status'] | undefined,
): TranscriptSpeaker | undefined => {
  const speakerId = nonEmptyString(speaker?.speaker_id) ?? nonEmptyString(flatSpeakerId);
  if (!speakerId) return undefined;

  return {
    ...speaker,
    speaker_id: speakerId,
    local_label: nonEmptyString(speaker?.local_label) ?? nonEmptyString(flatLocalLabel),
    display_name: nonEmptyString(speaker?.display_name) ?? nonEmptyString(flatDisplayName),
    confidence: finiteNumber(speaker?.confidence) ?? finiteNumber(flatConfidence),
    status: speaker?.status ?? flatStatus,
  };
};

const normalizeDiarization = (
  nested: TranscriptDiarizationMetadata | undefined,
  update: TranscriptUpdate,
): TranscriptDiarizationMetadata | undefined => {
  const provider = nonEmptyString(nested?.provider)
    ?? nonEmptyString(update.diarization_provider);
  const model = nonEmptyString(nested?.model)
    ?? nonEmptyString(update.diarization_model);
  const modelRevision = nonEmptyString(nested?.model_revision)
    ?? nonEmptyString(update.diarization_model_revision);
  const revisionValue = finiteNumber(nested?.revision)
    ?? finiteNumber(update.diarization_revision);
  const windowId = nonEmptyString(nested?.window_id)
    ?? nonEmptyString(update.diarization_window_id);
  const windowStartValue = finiteNumber(nested?.window_start_frame)
    ?? finiteNumber(update.diarization_window_start_frame);
  const windowEndValue = finiteNumber(nested?.window_end_frame)
    ?? finiteNumber(update.diarization_window_end_frame);
  const latencyValue = finiteNumber(nested?.latency_ms)
    ?? finiteNumber(update.diarization_latency_ms);
  const status = nested?.status ?? update.diarization_status;
  const hasMetadata = nested !== undefined
    || provider !== undefined
    || model !== undefined
    || modelRevision !== undefined
    || revisionValue !== undefined
    || windowId !== undefined
    || windowStartValue !== undefined
    || windowEndValue !== undefined
    || status !== undefined
    || latencyValue !== undefined;
  if (!hasMetadata) return undefined;

  return {
    ...nested,
    provider,
    model,
    model_revision: modelRevision,
    revision: revisionValue === undefined
      ? undefined
      : nonNegativeInteger(revisionValue, 0),
    window_id: windowId,
    window_start_frame: windowStartValue === undefined
      ? undefined
      : nonNegativeInteger(windowStartValue, 0),
    window_end_frame: windowEndValue === undefined
      ? undefined
      : nonNegativeInteger(windowEndValue, 0),
    status,
    latency_ms: latencyValue === undefined
      ? undefined
      : nonNegativeRoundedInteger(latencyValue, 0),
  };
};

/**
 * Normalize a legacy or v1 transcript update without mutating the payload.
 *
 * Legacy second-based timing remains available while the canonical v1 timing
 * is exposed in milliseconds. Missing revision metadata is treated as revision
 * zero, which lets the reducer use one comparison path for both provider and
 * locally corrected events.
 */
export function normalizeTranscriptEvent(update: TranscriptUpdate): NormalizedTranscriptUpdate {
  const eventKind = nonEmptyString(update.event_kind)
    ?? (update.is_partial ? 'partial' : 'final');
  const explicitStartMs = finiteNumber(update.start_ms);
  const audioStartSeconds = explicitStartMs === undefined
    ? finiteNumber(update.audio_start_time) ?? finiteNumber(update.chunk_start_time) ?? 0
    : explicitStartMs / 1_000;
  const startMs = nonNegativeRoundedInteger(explicitStartMs ?? audioStartSeconds * 1_000, 0);
  const durationSeconds = finiteNumber(update.duration) ?? 0;
  const explicitEndMs = finiteNumber(update.end_ms);
  const legacyAudioEndSeconds = finiteNumber(update.audio_end_time);
  const audioEndSeconds = explicitEndMs === undefined
    ? legacyAudioEndSeconds !== undefined && legacyAudioEndSeconds >= audioStartSeconds
      ? legacyAudioEndSeconds
      : audioStartSeconds + durationSeconds
    : explicitEndMs / 1_000;
  const endMs = nonNegativeRoundedInteger(explicitEndMs ?? audioEndSeconds * 1_000, startMs);
  const normalizedEndMs = Math.max(startMs, endMs);
  const normalizedAudioStartSeconds = startMs / 1_000;
  const normalizedAudioEndSeconds = normalizedEndMs / 1_000;
  const speaker = normalizeSpeaker(
    update.speaker,
    update.speaker_id,
    update.speaker_local_label,
    update.speaker_display_name,
    update.speaker_confidence,
    update.speaker_status,
  );
  const utteranceId = nonEmptyString(update.utterance_id);
  const audioSource = nonEmptyString(update.audio_source) ?? nonEmptyString(update.source);
  const source = nonEmptyString(update.source) ?? audioSource ?? 'unknown';
  const asrProvider = nonEmptyString(update.asr?.provider)
    ?? nonEmptyString(update.asr_provider)
    ?? nonEmptyString(update.provider);
  const asrModel = nonEmptyString(update.asr?.model)
    ?? nonEmptyString(update.asr_model)
    ?? nonEmptyString(update.model);
  const asrConfidence = finiteNumber(update.asr?.confidence)
    ?? finiteNumber(update.asr_confidence);
  const rawAsrLatencyMs = finiteNumber(update.asr?.latency_ms)
    ?? finiteNumber(update.asr_latency_ms)
    ?? finiteNumber(update.latency_ms);
  const asrLatencyMs = rawAsrLatencyMs === undefined
    ? undefined
    : nonNegativeRoundedInteger(rawAsrLatencyMs, 0);
  const hasAsrMetadata = update.asr !== undefined
    || asrProvider !== undefined
    || asrModel !== undefined
    || asrConfidence !== undefined
    || asrLatencyMs !== undefined;
  const asr = hasAsrMetadata
    ? {
        ...update.asr,
        provider: asrProvider,
        model: asrModel,
        confidence: asrConfidence,
        latency_ms: asrLatencyMs,
      }
    : undefined;
  const diarization = normalizeDiarization(update.diarization, update);

  return {
    ...update,
    source,
    schema_version: nonNegativeInteger(update.schema_version, 0),
    event_id: nonEmptyString(update.event_id),
    meeting_id: nonEmptyString(update.meeting_id),
    session_id: nonEmptyString(update.session_id),
    utterance_id: utteranceId,
    revision: nonNegativeInteger(update.revision, 0),
    event_kind: eventKind,
    is_stable: update.is_stable ?? (!update.is_partial && eventKind !== 'partial'),
    start_ms: startMs,
    end_ms: normalizedEndMs,
    audio_start_time: normalizedAudioStartSeconds,
    audio_end_time: normalizedAudioEndSeconds,
    duration: Math.max(0, normalizedAudioEndSeconds - normalizedAudioStartSeconds),
    language: nonEmptyString(update.language),
    audio_source: audioSource ?? source,
    speaker,
    speaker_id: speaker?.speaker_id,
    speaker_local_label: speaker?.local_label,
    speaker_display_name: speaker?.display_name,
    speaker_confidence: speaker?.confidence,
    speaker_status: speaker?.status,
    asr,
    asr_provider: asrProvider,
    asr_model: asrModel,
    asr_confidence: asrConfidence,
    asr_latency_ms: asrLatencyMs,
    diarization,
    diarization_provider: diarization?.provider,
    diarization_model: diarization?.model,
    diarization_model_revision: diarization?.model_revision,
    diarization_revision: diarization?.revision,
    diarization_window_id: diarization?.window_id,
    diarization_window_start_frame: diarization?.window_start_frame,
    diarization_window_end_frame: diarization?.window_end_frame,
    diarization_status: diarization?.status,
    diarization_latency_ms: diarization?.latency_ms,
    provider: asrProvider,
    model: asrModel,
    latency_ms: asrLatencyMs,
  };
}

/** Resolve a speaker ID regardless of whether the producer used flat or nested metadata. */
export function getTranscriptSpeakerId(
  transcript: Pick<Transcript, 'speaker' | 'speaker_id'>,
): string | undefined {
  return nonEmptyString(transcript.speaker_id)
    ?? nonEmptyString(transcript.speaker?.speaker_id);
}

/** Convert a backend update into the frontend's stored/display transcript shape. */
export function transcriptUpdateToTranscript(
  update: TranscriptUpdate,
  options: TranscriptConversionOptions = {},
): Transcript {
  const normalized = normalizeTranscriptEvent(update);
  const id = normalized.utterance_id
    ?? options.legacyId
    ?? normalized.event_id
    ?? `legacy:${normalized.sequence_id}:${normalized.timestamp}`;

  return {
    id,
    text: normalized.text,
    timestamp: normalized.timestamp,
    source: normalized.source,
    sequence_id: normalized.sequence_id,
    chunk_start_time: normalized.chunk_start_time,
    is_partial: normalized.is_partial,
    confidence: normalized.confidence,
    audio_start_time: normalized.audio_start_time,
    audio_end_time: normalized.audio_end_time,
    duration: normalized.duration,
    schema_version: normalized.schema_version,
    event_id: normalized.event_id,
    meeting_id: normalized.meeting_id,
    session_id: normalized.session_id,
    utterance_id: normalized.utterance_id,
    revision: normalized.revision,
    event_kind: normalized.event_kind,
    is_stable: normalized.is_stable,
    start_ms: normalized.start_ms,
    end_ms: normalized.end_ms,
    language: normalized.language,
    audio_source: normalized.audio_source,
    speaker: normalized.speaker,
    speaker_id: normalized.speaker_id,
    speaker_local_label: normalized.speaker_local_label,
    speaker_display_name: normalized.speaker_display_name,
    speaker_confidence: normalized.speaker_confidence,
    speaker_status: normalized.speaker_status,
    asr: normalized.asr,
    asr_provider: normalized.asr_provider,
    asr_model: normalized.asr_model,
    asr_confidence: normalized.asr_confidence,
    asr_latency_ms: normalized.asr_latency_ms,
    diarization: normalized.diarization,
    diarization_provider: normalized.diarization_provider,
    diarization_model: normalized.diarization_model,
    diarization_model_revision: normalized.diarization_model_revision,
    diarization_revision: normalized.diarization_revision,
    diarization_window_id: normalized.diarization_window_id,
    diarization_window_start_frame: normalized.diarization_window_start_frame,
    diarization_window_end_frame: normalized.diarization_window_end_frame,
    diarization_status: normalized.diarization_status,
    diarization_latency_ms: normalized.diarization_latency_ms,
    provider: normalized.provider,
    model: normalized.model,
    latency_ms: normalized.latency_ms,
    replaces_event_id: normalized.replaces_event_id,
    provider_event_id: normalized.provider_event_id,
    created_at: normalized.created_at,
    trace_id: normalized.trace_id,
  };
}

const transcriptStartMs = (transcript: Transcript): number | undefined => {
  const startMs = finiteNumber(transcript.start_ms);
  if (startMs !== undefined) return startMs;

  const audioStartSeconds = finiteNumber(transcript.audio_start_time);
  return audioStartSeconds === undefined ? undefined : audioStartSeconds * 1_000;
};

/** Chronological ordering shared by live state, reload, and overlay consumers. */
export function sortTranscripts(transcripts: readonly Transcript[]): Transcript[] {
  return transcripts
    .map((transcript, index) => ({ transcript, index }))
    .sort((left, right) => {
      const leftStart = transcriptStartMs(left.transcript);
      const rightStart = transcriptStartMs(right.transcript);

      if (leftStart !== undefined || rightStart !== undefined) {
        if (leftStart === undefined) return 1;
        if (rightStart === undefined) return -1;
        if (leftStart !== rightStart) return leftStart - rightStart;
      }

      const leftSequence = finiteNumber(left.transcript.sequence_id);
      const rightSequence = finiteNumber(right.transcript.sequence_id);
      if (leftSequence !== undefined || rightSequence !== undefined) {
        if (leftSequence === undefined) return 1;
        if (rightSequence === undefined) return -1;
        if (leftSequence !== rightSequence) return leftSequence - rightSequence;
      }

      return left.index - right.index;
    })
    .map(({ transcript }) => transcript);
}

const eventPriority = (transcript: Transcript): number => {
  if (transcript.event_kind === 'retraction') return 5;
  if (transcript.event_kind === 'speaker_update' || transcript.event_kind === 'language_update') {
    return 4;
  }
  if (transcript.event_kind === 'correction') return 3;
  if (transcript.is_stable || transcript.event_kind === 'final') return 2;
  if (transcript.event_kind === 'partial' || transcript.is_partial) return 0;
  return 1;
};

const revisionOf = (transcript: Transcript): number =>
  nonNegativeInteger(transcript.revision, 0);

export const compareTranscriptVersions = (left: Transcript, right: Transcript): number => {
  const revisionDifference = revisionOf(left) - revisionOf(right);
  if (revisionDifference !== 0) return revisionDifference;

  const stabilityDifference = Number(Boolean(left.is_stable)) - Number(Boolean(right.is_stable));
  if (stabilityDifference !== 0) return stabilityDifference;

  const priorityDifference = eventPriority(left) - eventPriority(right);
  if (priorityDifference !== 0) return priorityDifference;

  const compareString = (leftValue: string | undefined, rightValue: string | undefined): number => {
    const normalizedLeft = leftValue ?? '';
    const normalizedRight = rightValue ?? '';
    if (normalizedLeft === normalizedRight) return 0;
    return normalizedLeft > normalizedRight ? 1 : -1;
  };
  const createdAtDifference = compareString(left.created_at, right.created_at);
  return createdAtDifference !== 0
    ? createdAtDifference
    : compareString(left.event_id, right.event_id);
};

/** Compare immutable revision content, excluding delivery identity/timestamps. */
export function transcriptEventPayloadEquals(
  left: TranscriptUpdate,
  right: TranscriptUpdate,
): boolean {
  const normalizedLeft = normalizeTranscriptEvent(left);
  const normalizedRight = normalizeTranscriptEvent(right);
  const signature = (event: NormalizedTranscriptUpdate) => JSON.stringify([
    event.schema_version,
    event.session_id,
    event.utterance_id,
    event.revision,
    event.event_kind,
    event.is_stable,
    event.text,
    event.timestamp,
    event.sequence_id,
    event.start_ms,
    event.end_ms,
    event.audio_start_time,
    event.audio_end_time,
    event.duration,
    event.audio_source,
    event.speaker_id,
    event.speaker_local_label,
    event.speaker_display_name,
    event.speaker_confidence,
    event.speaker_status,
    event.language,
    event.asr_provider,
    event.asr_model,
    event.asr_confidence,
    event.asr_latency_ms,
    event.diarization_provider,
    event.diarization_model,
    event.diarization_model_revision,
    event.diarization_revision,
    event.diarization_window_id,
    event.diarization_window_start_frame,
    event.diarization_window_end_frame,
    event.diarization_status,
    event.diarization_latency_ms,
    event.replaces_event_id,
    event.provider_event_id,
    event.trace_id,
  ]);
  return signature(normalizedLeft) === signature(normalizedRight);
}

/**
 * Keep one auditable payload per utterance revision.
 *
 * This differs from `upsertTranscript`, which materializes only the latest
 * revision for display. A stable/final payload may replace a partial payload
 * at the same revision; equal-priority conflicts use created_at/event_id as a
 * deterministic tie-breaker, matching Rust replay and recovery persistence.
 */
export function upsertTranscriptRevisionHistory(
  events: readonly TranscriptUpdate[],
  update: TranscriptUpdate,
): TranscriptUpdate[] {
  const incoming = normalizeTranscriptEvent(update);

  if (incoming.event_id) {
    const delivered = events.find((event) => event.event_id === incoming.event_id);
    if (delivered) {
      if (!transcriptEventPayloadEquals(delivered, incoming)) {
        console.warn('Transcript provider reused event_id with different payload; ignored:', {
          event_id: incoming.event_id,
          utterance_id: incoming.utterance_id,
          revision: incoming.revision,
        });
      }
      return events as TranscriptUpdate[];
    }
  }

  if (!incoming.utterance_id) {
    return [...events, incoming];
  }

  const existingIndex = events.findIndex((event) => (
    event.utterance_id === incoming.utterance_id
    && (event.revision ?? 0) === incoming.revision
  ));
  if (existingIndex < 0) {
    return [...events, incoming];
  }

  const existing = normalizeTranscriptEvent(events[existingIndex]);
  const exactDuplicate = transcriptEventPayloadEquals(existing, incoming);
  if (exactDuplicate) return events as TranscriptUpdate[];

  const incomingDisplay = transcriptUpdateToTranscript(incoming);
  const existingDisplay = transcriptUpdateToTranscript(existing);
  if (compareTranscriptVersions(incomingDisplay, existingDisplay) <= 0) {
    return events as TranscriptUpdate[];
  }

  const next = [...events];
  next[existingIndex] = incoming;
  return next;
}

/** Merge persisted snapshots with events that arrived while reload I/O ran. */
export function mergeTranscriptRevisionHistories(
  ...histories: ReadonlyArray<readonly TranscriptUpdate[]>
): TranscriptUpdate[] {
  return histories.reduce<TranscriptUpdate[]>(
    (merged, history) => history.reduce(
      (events, update) => upsertTranscriptRevisionHistory(events, update),
      merged,
    ),
    [],
  );
}

/**
 * Append a legacy event, or idempotently upsert a revisable event.
 *
 * The highest `revision` wins for a given `utterance_id`. A stable/final event
 * may replace a partial event at the same revision, while an equal-priority
 * duplicate is ignored. Existing duplicate rows are compacted as a defensive
 * repair for state restored from older app versions.
 */
export function upsertTranscript(
  transcripts: readonly Transcript[],
  update: TranscriptUpdate,
): Transcript[] {
  const normalized = normalizeTranscriptEvent(update);
  const incoming = transcriptUpdateToTranscript(normalized, {
    legacyId: `legacy:${transcripts.length}:${normalized.sequence_id}:${normalized.timestamp}`,
  });

  // event_id is a delivery identity across the whole stream. Reusing it for a
  // different revision is a provider protocol error; first delivery wins,
  // matching Rust replay and RecordingSaver behavior.
  if (
    normalized.event_id
    && transcripts.some((transcript) => transcript.event_id === normalized.event_id)
  ) {
    return transcripts as Transcript[];
  }

  if (!normalized.utterance_id) {
    return sortTranscripts([...transcripts, incoming]);
  }

  const matchingIndexes: number[] = [];
  let existingWinner: Transcript | undefined;

  transcripts.forEach((transcript, index) => {
    if (transcript.utterance_id !== normalized.utterance_id) return;
    matchingIndexes.push(index);
    if (!existingWinner || compareTranscriptVersions(transcript, existingWinner) > 0) {
      existingWinner = transcript;
    }
  });

  if (!existingWinner) {
    // A retraction is a tombstone, never a visible transcript row. Callers
    // that keep the complete revision history use `upsertTranscripts`, which
    // also prevents a delayed older event from resurrecting this utterance.
    if (normalized.event_kind === 'retraction') {
      return transcripts as Transcript[];
    }
    return sortTranscripts([...transcripts, incoming]);
  }

  const incomingWins = compareTranscriptVersions(incoming, existingWinner) > 0;
  if (!incomingWins && matchingIndexes.length === 1) {
    return transcripts as Transcript[];
  }

  if (incomingWins && normalized.event_kind === 'retraction') {
    const matchingIndexSet = new Set(matchingIndexes);
    return sortTranscripts(
      transcripts.filter((_, index) => !matchingIndexSet.has(index)),
    );
  }

  const winner = incomingWins
    ? { ...existingWinner, ...incoming, id: normalized.utterance_id }
    : existingWinner;
  const matchingIndexSet = new Set(matchingIndexes);
  const compacted = transcripts.filter((_, index) => !matchingIndexSet.has(index));

  return sortTranscripts([...compacted, winner]);
}

/** Replay a stream using the same reducer semantics as live delivery. */
export function upsertTranscripts(
  transcripts: readonly Transcript[],
  updates: readonly TranscriptUpdate[],
): Transcript[] {
  // Replay from immutable history when no pre-existing display state is
  // supplied. Keeping tombstones in the winner map until the final filter is
  // what prevents an out-of-order stale event from resurrecting a retracted
  // utterance.
  if (transcripts.length === 0) {
    const acceptedHistory = mergeTranscriptRevisionHistories(updates);
    const legacy: Transcript[] = [];
    const winners = new Map<string, Transcript>();

    acceptedHistory.forEach((update, index) => {
      const normalized = normalizeTranscriptEvent(update);
      const candidate = transcriptUpdateToTranscript(normalized, {
        legacyId: `legacy:${index}:${normalized.sequence_id}:${normalized.timestamp}`,
      });
      if (!normalized.utterance_id) {
        legacy.push(candidate);
        return;
      }

      const existing = winners.get(normalized.utterance_id);
      if (!existing || compareTranscriptVersions(candidate, existing) > 0) {
        winners.set(normalized.utterance_id, candidate);
      }
    });

    return sortTranscripts([
      ...legacy,
      ...[...winners.values()].filter((winner) => winner.event_kind !== 'retraction'),
    ]);
  }

  let acceptedHistory: TranscriptUpdate[] = [];
  let materialized = [...transcripts];
  for (const update of updates) {
    const nextHistory = upsertTranscriptRevisionHistory(acceptedHistory, update);
    if (nextHistory === acceptedHistory) continue;
    acceptedHistory = nextHistory;
    materialized = upsertTranscript(materialized, update);
  }
  return materialized;
}
