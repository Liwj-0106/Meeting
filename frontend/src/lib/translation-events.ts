/**
 * Revision-bound bilingual text translation reducer.
 *
 * A translation is displayable only when its source event ID, source
 * revision, and source text hash all match the current transcript source.
 * Provider token deltas stay outside this contract; every accepted event is a
 * complete translated snapshot, tombstone, or sanitized failure.
 */

export const TRANSLATION_EVENT_SCHEMA_VERSION = 1;

export type TranslationSourceKind =
  | 'partial'
  | 'final'
  | 'correction'
  | 'speaker_update'
  | 'language_update'
  | 'retraction';

export type TranslationEventKind = 'snapshot' | 'retraction' | 'error';
export type TranslationStatus = 'partial' | 'final' | 'reused' | 'retracted' | 'failed';
export type TranslationErrorCode =
  | 'provider_rejected'
  | 'provider_failed'
  | 'invalid_response';

export interface TranslationSourceBinding {
  meeting_id: string;
  event_id: string;
  utterance_id: string;
  revision: number;
  /** Lowercase SHA-256 hex of the exact normalized source text. */
  text_hash: string;
}

export interface TranslationSourceSnapshot extends TranslationSourceBinding {
  event_kind: TranslationSourceKind;
  is_stable: boolean;
  source_language: string;
  speaker_id?: string;
}

export interface GlossaryVersionBinding {
  glossary_id: string;
  version: number;
  content_hash: string;
}

/** Inputs which may change a translation while the source triple is stable. */
export interface TranslationRequestFingerprint {
  source_language: string;
  target_language: string;
  provider: string;
  model?: string;
  glossary?: GlossaryVersionBinding;
}

export interface TranslationFailure {
  code: TranslationErrorCode;
  message: string;
  retryable: boolean;
}

export interface TranslationEvent {
  schema_version: number;
  translation_event_id: string;
  source: TranslationSourceBinding;
  source_kind: TranslationSourceKind;
  translation_revision: number;
  generation: number;
  event_kind: TranslationEventKind;
  status: TranslationStatus;
  request_fingerprint: TranslationRequestFingerprint;
  source_language: string;
  target_language: string;
  translated_text?: string;
  provider?: string;
  model?: string;
  glossary?: GlossaryVersionBinding;
  speaker_id?: string;
  reused_from_event_id?: string;
  latency_ms?: number;
  error?: TranslationFailure;
  created_at_ms: number;
}

export interface TranslationReducerState {
  /** Current transcript source per meeting/utterance. */
  sources: Record<string, TranslationSourceSnapshot>;
  /** Current translation projection per meeting/utterance/target language. */
  latest: Record<string, TranslationEvent>;
  /** Accepted immutable delivery IDs and their payload signatures. */
  seen_source_events: Record<string, string>;
  seen_translation_events: Record<string, string>;
  /** Durable monotonic gates survive source changes which clear `latest`. */
  high_watermarks: Record<string, TranslationHighWatermark>;
}

export interface TranslationHighWatermark {
  generation: number;
  translation_revision: number;
}

export type TranslationReducerOutcome =
  | 'applied'
  | 'duplicate'
  | 'stale'
  | 'unbound'
  | 'invalid'
  | 'event_id_conflict';

export interface TranslationReducerResult {
  state: TranslationReducerState;
  outcome: TranslationReducerOutcome;
}

export const createTranslationReducerState = (): TranslationReducerState => ({
  sources: {},
  latest: {},
  seen_source_events: {},
  seen_translation_events: {},
  high_watermarks: {},
});

const sourceKey = (source: Pick<TranslationSourceBinding, 'meeting_id' | 'utterance_id'>): string =>
  `${source.meeting_id}\u0000${source.utterance_id}`;

const translationKey = (
  source: Pick<TranslationSourceBinding, 'meeting_id' | 'utterance_id'>,
  targetLanguage: string,
): string => `${sourceKey(source)}\u0000${targetLanguage.toLowerCase()}`;

const isIdentifier = (value: unknown): value is string =>
  typeof value === 'string'
  && value.length >= 1
  && value.length <= 256
  && !/[\u0000-\u001f\u007f]/u.test(value);

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const isProvider = (value: unknown): value is string =>
  typeof value === 'string'
  && value.length >= 1
  && value.length <= 128
  && !/[\u0000-\u001f\u007f]/u.test(value);

const isLanguageTag = (value: unknown): value is string =>
  typeof value === 'string' && /^[A-Za-z0-9-]{1,35}$/u.test(value);

const isHash = (value: unknown): value is string =>
  typeof value === 'string' && /^[0-9a-f]{64}$/u.test(value);

const isNonNegativeInteger = (value: unknown): value is number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;

const isPositiveInteger = (value: unknown): value is number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 1;

const sourcePriority = (kind: TranslationSourceKind): number => {
  switch (kind) {
    case 'partial': return 1;
    case 'final': return 2;
    case 'correction': return 3;
    case 'speaker_update':
    case 'language_update': return 4;
    case 'retraction': return 5;
  }
};

const sourceKinds = new Set<TranslationSourceKind>([
  'partial',
  'final',
  'correction',
  'speaker_update',
  'language_update',
  'retraction',
]);

const validSourceBinding = (source: unknown): source is TranslationSourceBinding =>
  isRecord(source)
  && isIdentifier(source.meeting_id)
  && isIdentifier(source.event_id)
  && isIdentifier(source.utterance_id)
  && isNonNegativeInteger(source.revision)
  && isHash(source.text_hash);

const sameSourceVersion = (
  left: TranslationSourceBinding,
  right: TranslationSourceBinding,
): boolean =>
  left.meeting_id === right.meeting_id
  && left.utterance_id === right.utterance_id
  && left.event_id === right.event_id
  && left.revision === right.revision
  && left.text_hash === right.text_hash;

const sourceSignature = (source: TranslationSourceSnapshot): string => JSON.stringify([
  source.meeting_id,
  source.event_id,
  source.utterance_id,
  source.revision,
  source.text_hash,
  source.event_kind,
  source.is_stable,
  source.source_language,
  source.speaker_id ?? null,
]);

const translationSignature = (event: TranslationEvent): string => JSON.stringify([
  event.schema_version,
  event.translation_event_id,
  event.source.meeting_id,
  event.source.event_id,
  event.source.utterance_id,
  event.source.revision,
  event.source.text_hash,
  event.source_kind,
  event.translation_revision,
  event.generation,
  event.event_kind,
  event.status,
  event.request_fingerprint.source_language,
  event.request_fingerprint.target_language,
  event.request_fingerprint.provider,
  event.request_fingerprint.model ?? null,
  event.request_fingerprint.glossary?.glossary_id ?? null,
  event.request_fingerprint.glossary?.version ?? null,
  event.request_fingerprint.glossary?.content_hash ?? null,
  event.source_language,
  event.target_language,
  event.translated_text ?? null,
  event.provider ?? null,
  event.model ?? null,
  event.glossary?.glossary_id ?? null,
  event.glossary?.version ?? null,
  event.glossary?.content_hash ?? null,
  event.speaker_id ?? null,
  event.reused_from_event_id ?? null,
  event.latency_ms ?? null,
  event.error?.code ?? null,
  event.error?.message ?? null,
  event.error?.retryable ?? null,
  event.created_at_ms,
]);

const validSourceSnapshot = (source: unknown): source is TranslationSourceSnapshot => {
  if (!validSourceBinding(source)) return false;
  const raw = source as TranslationSourceBinding & Record<string, unknown>;
  if (!sourceKinds.has(raw.event_kind as TranslationSourceKind)) return false;
  const eventKind = raw.event_kind as TranslationSourceKind;
  return typeof raw.is_stable === 'boolean'
    && (eventKind === 'partial' ? !raw.is_stable : raw.is_stable)
    && isLanguageTag(raw.source_language)
    && (raw.speaker_id === undefined || isIdentifier(raw.speaker_id));
};

const validGlossary = (glossary: unknown): glossary is GlossaryVersionBinding | undefined =>
  glossary === undefined
  || (isRecord(glossary)
    && isIdentifier(glossary.glossary_id)
    && isPositiveInteger(glossary.version)
    && isHash(glossary.content_hash));

const sameGlossary = (
  left: GlossaryVersionBinding | undefined,
  right: GlossaryVersionBinding | undefined,
): boolean => left?.glossary_id === right?.glossary_id
  && left?.version === right?.version
  && left?.content_hash === right?.content_hash;

const validRequestFingerprint = (
  fingerprint: unknown,
): fingerprint is TranslationRequestFingerprint => isRecord(fingerprint)
  && isLanguageTag(fingerprint.source_language)
  && isLanguageTag(fingerprint.target_language)
  && fingerprint.source_language.toLowerCase() !== fingerprint.target_language.toLowerCase()
  && isProvider(fingerprint.provider)
  && (fingerprint.model === undefined || isIdentifier(fingerprint.model))
  && validGlossary(fingerprint.glossary);

const validFailure = (failure: unknown): failure is TranslationFailure => isRecord(failure)
  && ['provider_rejected', 'provider_failed', 'invalid_response'].includes(failure.code as string)
  && typeof failure.retryable === 'boolean'
  && typeof failure.message === 'string'
  && failure.message.trim().length > 0
  && failure.message.length <= 512
  && !/[\u0000-\u001f\u007f]/u.test(failure.message);

const validCompleteText = (value: unknown): value is string => typeof value === 'string'
  && value.trim().length > 0
  && value.length <= 65_536
  && !value.includes('\u0000');

const eventKinds = new Set<TranslationEventKind>(['snapshot', 'retraction', 'error']);
const translationStatuses = new Set<TranslationStatus>([
  'partial',
  'final',
  'reused',
  'retracted',
  'failed',
]);

const validTranslationEvent = (value: unknown): value is TranslationEvent => {
  if (!isRecord(value)) return false;
  const event = value;
  if ('delta' in event || 'token_delta' in event) return false;
  if (
    event.schema_version !== TRANSLATION_EVENT_SCHEMA_VERSION
    || !isIdentifier(event.translation_event_id)
    || !validSourceBinding(event.source)
    || !sourceKinds.has(event.source_kind as TranslationSourceKind)
    || !isPositiveInteger(event.translation_revision)
    || !isPositiveInteger(event.generation)
    || !eventKinds.has(event.event_kind as TranslationEventKind)
    || !translationStatuses.has(event.status as TranslationStatus)
    || !validRequestFingerprint(event.request_fingerprint)
    || !isLanguageTag(event.source_language)
    || !isLanguageTag(event.target_language)
    || event.source_language.toLowerCase() === event.target_language.toLowerCase()
    || event.source_language.toLowerCase()
      !== event.request_fingerprint.source_language.toLowerCase()
    || event.target_language.toLowerCase()
      !== event.request_fingerprint.target_language.toLowerCase()
    || !validGlossary(event.glossary)
    || !sameGlossary(event.glossary, event.request_fingerprint.glossary)
    || !isNonNegativeInteger(event.created_at_ms)
    || (event.speaker_id !== undefined && !isIdentifier(event.speaker_id))
    || (event.latency_ms !== undefined && !isNonNegativeInteger(event.latency_ms))
  ) {
    return false;
  }

  if (event.status === 'partial' || event.status === 'final' || event.status === 'reused') {
    const sourceKindMatchesStatus = event.status === 'partial'
      ? event.source_kind === 'partial'
      : (event.status === 'reused'
        ? event.source_kind === 'speaker_update'
        : ['final', 'correction', 'speaker_update', 'language_update'].includes(
          event.source_kind as string,
        ));
    return event.event_kind === 'snapshot'
      && sourceKindMatchesStatus
      && validCompleteText(event.translated_text)
      && isProvider(event.provider)
      && event.provider === event.request_fingerprint.provider
      && event.model === event.request_fingerprint.model
      && event.error === undefined
      && (event.status === 'reused'
        ? isIdentifier(event.reused_from_event_id)
        : event.reused_from_event_id === undefined);
  }

  if (event.status === 'retracted') {
    return event.event_kind === 'retraction'
      && event.source_kind === 'retraction'
      && event.translated_text === undefined
      && event.provider === undefined
      && event.model === undefined
      && event.reused_from_event_id === undefined
      && event.latency_ms === undefined
      && event.error === undefined;
  }

  if (event.status === 'failed') {
    return event.event_kind === 'error'
      && event.source_kind !== 'retraction'
      && event.translated_text === undefined
      && isProvider(event.provider)
      && event.provider === event.request_fingerprint.provider
      && event.model === event.request_fingerprint.model
      && event.reused_from_event_id === undefined
      && event.latency_ms === undefined
      && validFailure(event.error);
  }

  return false;
};

const unchanged = (
  state: TranslationReducerState,
  outcome: TranslationReducerOutcome,
): TranslationReducerResult => ({ state, outcome });

/**
 * Register the current transcript source before accepting a translation.
 * A newer source immediately hides every translation still bound to the old
 * source triple, including delayed provider results.
 */
export function registerTranslationSource(
  state: TranslationReducerState,
  source: unknown,
): TranslationReducerResult {
  if (!validSourceSnapshot(source)) return unchanged(state, 'invalid');

  const signature = sourceSignature(source);
  const seenSignature = state.seen_source_events[source.event_id];
  if (seenSignature !== undefined) {
    return unchanged(state, seenSignature === signature ? 'duplicate' : 'event_id_conflict');
  }

  const key = sourceKey(source);
  const current = state.sources[key];
  if (current) {
    if (source.revision < current.revision) return unchanged(state, 'stale');
    if (source.revision === current.revision) {
      if (sameSourceVersion(source, current)) {
        return unchanged(state, sourceSignature(current) === signature ? 'duplicate' : 'event_id_conflict');
      }
      if (sourcePriority(source.event_kind) <= sourcePriority(current.event_kind)) {
        return unchanged(state, 'stale');
      }
    }
  }

  const latest = Object.fromEntries(
    Object.entries(state.latest).filter(([, event]) => sourceKey(event.source) !== key),
  );
  return {
    outcome: 'applied',
    state: {
      sources: { ...state.sources, [key]: source },
      latest,
      seen_source_events: {
        ...state.seen_source_events,
        [source.event_id]: signature,
      },
      seen_translation_events: state.seen_translation_events,
      high_watermarks: state.high_watermarks,
    },
  };
}

/**
 * Apply one complete translation snapshot to the current projection.
 * Responses arriving before their source, after a correction, or after a
 * retraction are rejected rather than queued for speculative display.
 */
export function reduceTranslationEvent(
  state: TranslationReducerState,
  event: unknown,
): TranslationReducerResult {
  if (!validTranslationEvent(event)) return unchanged(state, 'invalid');

  const signature = translationSignature(event);
  const seenSignature = state.seen_translation_events[event.translation_event_id];
  if (seenSignature !== undefined) {
    return unchanged(state, seenSignature === signature ? 'duplicate' : 'event_id_conflict');
  }

  const currentSource = state.sources[sourceKey(event.source)];
  if (!currentSource || !sameSourceVersion(event.source, currentSource)) {
    return unchanged(state, 'unbound');
  }
  if (event.source_language.toLowerCase() !== currentSource.source_language.toLowerCase()) {
    return unchanged(state, 'unbound');
  }
  if (event.source_kind !== currentSource.event_kind) return unchanged(state, 'unbound');
  if (currentSource.event_kind === 'retraction' && event.status !== 'retracted') {
    return unchanged(state, 'unbound');
  }
  if (currentSource.event_kind !== 'retraction' && event.status === 'retracted') {
    return unchanged(state, 'unbound');
  }

  const key = translationKey(event.source, event.target_language);
  const highWatermark = state.high_watermarks[key];
  if (
    highWatermark
    && (
      event.generation <= highWatermark.generation
      || event.translation_revision <= highWatermark.translation_revision
    )
  ) {
    return unchanged(state, 'stale');
  }

  return {
    outcome: 'applied',
    state: {
      sources: state.sources,
      latest: { ...state.latest, [key]: event },
      seen_source_events: state.seen_source_events,
      seen_translation_events: {
        ...state.seen_translation_events,
        [event.translation_event_id]: signature,
      },
      high_watermarks: {
        ...state.high_watermarks,
        [key]: {
          generation: event.generation,
          translation_revision: event.translation_revision,
        },
      },
    },
  };
}

/** Return only complete snapshots still bound to the current source. */
export function selectVisibleTranslations(state: TranslationReducerState): TranslationEvent[] {
  return Object.values(state.latest).filter((event) => {
    const source = state.sources[sourceKey(event.source)];
    return source !== undefined
      && sameSourceVersion(event.source, source)
      && (event.status === 'partial' || event.status === 'final' || event.status === 'reused');
  });
}
