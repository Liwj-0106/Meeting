import type {
  LiveSummaryEvidence,
  LiveSummaryItem,
  LiveSummaryItemKind,
  LiveSummaryItemStatus,
  LiveSummaryScope,
  LiveSummarySnapshot,
  LiveSummaryStateEvent,
} from '@/services/liveSummaryService';

export type MeetingAssistantMode = 'captions' | 'highlights' | 'actions';

export interface LiveSummaryOverlayState {
  snapshot: LiveSummarySnapshot | null;
  eventKind: LiveSummaryStateEvent['kind'] | null;
}

export type LiveSummaryOverlayOutcome =
  | 'applied'
  | 'duplicate'
  | 'stale'
  | 'scope_mismatch'
  | 'invalid';

export interface LiveSummaryOverlayReduction {
  state: LiveSummaryOverlayState;
  outcome: LiveSummaryOverlayOutcome;
}

export type LiveSummaryBinding =
  | { status: 'bound'; snapshot: LiveSummarySnapshot }
  | { status: 'missing_transcript_scope' }
  | { status: 'waiting_for_summary' }
  | { status: 'scope_mismatch' };

const SUMMARY_SCHEMA_VERSION = 1;
const SUMMARY_EVENT_KINDS = new Set<LiveSummaryStateEvent['kind']>([
  'session_started',
  'transcript_accepted',
  'summary_committed',
  'summary_failed',
  'final_reconcile_requested',
]);
const ITEM_KINDS = new Set<LiveSummaryItemKind>([
  'topic',
  'decision',
  'action_item',
  'risk',
  'open_question',
]);
const ITEM_STATUSES = new Set<LiveSummaryItemStatus>([
  'active',
  'needs_review',
  'retracted',
]);
const LIFECYCLES = new Set<LiveSummarySnapshot['lifecycle']>([
  'active',
  'generating',
  'unavailable',
  'finalized',
  'error',
]);
const DISPATCH_STATES = new Set<LiveSummarySnapshot['dispatchState']>([
  'idle',
  'in_flight',
  'deferred',
]);
const EVENT_PRIORITY: Record<LiveSummaryStateEvent['kind'], number> = {
  session_started: 0,
  transcript_accepted: 1,
  final_reconcile_requested: 2,
  summary_failed: 3,
  summary_committed: 4,
};
const HASH_PATTERN = /^[0-9a-f]{64}$/i;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isSafeText(value: unknown, maxLength = 16_384): value is string {
  return typeof value === 'string' && value.length <= maxLength && !value.includes('\0');
}

function isIdentifier(value: unknown): value is string {
  return isSafeText(value, 256) && value.trim().length > 0;
}

function isCounter(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) >= 0;
}

function isOptionalText(value: unknown, maxLength = 4_096): value is string | undefined {
  return value === undefined || isSafeText(value, maxLength);
}

function isPublicError(value: unknown): boolean {
  if (!isRecord(value)) return false;
  return isIdentifier(value.code)
    && isSafeText(value.message, 1_024)
    && typeof value.retryable === 'boolean';
}

function parseScope(value: unknown): LiveSummaryScope | null {
  if (!isRecord(value) || !isIdentifier(value.id)) return null;
  if (value.kind !== 'meeting' && value.kind !== 'recording_session') return null;
  return { kind: value.kind, id: value.id };
}

function sameScope(left: LiveSummaryScope, right: LiveSummaryScope): boolean {
  return left.kind === right.kind && left.id === right.id;
}

function parseEvidence(value: unknown, expectedScope: LiveSummaryScope): LiveSummaryEvidence | null {
  if (!isRecord(value)) return null;
  const scope = parseScope(value.scope);
  if (
    !scope
    || !sameScope(scope, expectedScope)
    || !isIdentifier(value.utterance_id)
    || !isCounter(value.source_revision)
    || !isIdentifier(value.source_event_id)
    || typeof value.source_text_hash !== 'string'
    || !HASH_PATTERN.test(value.source_text_hash)
  ) {
    return null;
  }

  return {
    scope,
    utterance_id: value.utterance_id,
    source_revision: value.source_revision,
    source_event_id: value.source_event_id,
    source_text_hash: value.source_text_hash.toLowerCase(),
  };
}

function parseItem(value: unknown, expectedScope: LiveSummaryScope): LiveSummaryItem | null {
  if (!isRecord(value)) return null;
  if (
    !isIdentifier(value.item_id)
    || typeof value.kind !== 'string'
    || !ITEM_KINDS.has(value.kind as LiveSummaryItemKind)
    || !isSafeText(value.title, 2_048)
    || !isSafeText(value.body, 16_384)
    || typeof value.status !== 'string'
    || !ITEM_STATUSES.has(value.status as LiveSummaryItemStatus)
    || !isOptionalText(value.owner, 512)
    || !isOptionalText(value.due_at, 512)
    || !Array.isArray(value.evidence)
    || value.evidence.length > 512
  ) {
    return null;
  }

  const evidence = value.evidence.map((entry) => parseEvidence(entry, expectedScope));
  if (evidence.some((entry) => entry === null)) return null;

  return {
    item_id: value.item_id,
    kind: value.kind as LiveSummaryItemKind,
    title: value.title,
    body: value.body,
    owner: value.owner,
    due_at: value.due_at,
    status: value.status as LiveSummaryItemStatus,
    evidence: evidence as LiveSummaryEvidence[],
  };
}

function parseSnapshot(value: unknown): LiveSummarySnapshot | null {
  if (!isRecord(value)) return null;
  const scope = parseScope(value.scope);
  if (
    value.schemaVersion !== SUMMARY_SCHEMA_VERSION
    || !scope
    || typeof value.lifecycle !== 'string'
    || !LIFECYCLES.has(value.lifecycle as LiveSummarySnapshot['lifecycle'])
    || typeof value.dispatchState !== 'string'
    || !DISPATCH_STATES.has(value.dispatchState as LiveSummarySnapshot['dispatchState'])
    || !isCounter(value.transcriptCursor)
    || !isCounter(value.summaryRevision)
    || !isCounter(value.generation)
    || typeof value.finalized !== 'boolean'
    || typeof value.recovered !== 'boolean'
    || !isOptionalText(value.templateId, 256)
    || !isOptionalText(value.updatedAt, 256)
    || !Array.isArray(value.items)
    || value.items.length > 512
    || !isRecord(value.provider)
    || typeof value.provider.available !== 'boolean'
    || !isIdentifier(value.provider.provider)
    || !isOptionalText(value.provider.model, 512)
    || (value.provider.error !== undefined && !isPublicError(value.provider.error))
    || (value.error !== undefined && !isPublicError(value.error))
  ) {
    return null;
  }

  if (
    (scope.kind === 'meeting'
      && (value.meetingId !== scope.id || value.sessionScopeId !== undefined))
    || (scope.kind === 'recording_session'
      && (value.meetingId !== undefined || value.sessionScopeId !== scope.id))
  ) {
    return null;
  }

  const items = value.items.map((entry) => parseItem(entry, scope));
  if (items.some((entry) => entry === null)) return null;

  const lastCompleteRevision = value.lastCompleteRevision;
  if (lastCompleteRevision !== undefined) {
    const revisionScope = isRecord(lastCompleteRevision)
      ? parseScope(lastCompleteRevision.scope)
      : null;
    if (
      !isRecord(lastCompleteRevision)
      || !isIdentifier(lastCompleteRevision.summary_revision_id)
      || !revisionScope
      || !sameScope(revisionScope, scope)
      || !isCounter(lastCompleteRevision.revision)
      || !isCounter(lastCompleteRevision.generation)
      || !isCounter(lastCompleteRevision.transcript_cursor)
      || typeof lastCompleteRevision.snapshot_hash !== 'string'
      || !HASH_PATTERN.test(lastCompleteRevision.snapshot_hash)
      || (lastCompleteRevision.revision_type !== 'live' && lastCompleteRevision.revision_type !== 'final')
      || !isIdentifier(lastCompleteRevision.provider)
      || !isOptionalText(lastCompleteRevision.model, 512)
      || !isSafeText(lastCompleteRevision.created_at, 256)
      || !Array.isArray(lastCompleteRevision.items)
      || lastCompleteRevision.items.length > 512
      || lastCompleteRevision.revision > value.summaryRevision
      || lastCompleteRevision.generation > value.generation
      || lastCompleteRevision.transcript_cursor > value.transcriptCursor
    ) {
      return null;
    }
    const revisionItems = lastCompleteRevision.items
      .map((entry) => parseItem(entry, scope));
    if (revisionItems.some((entry) => entry === null)) return null;
  }

  return value as unknown as LiveSummarySnapshot;
}

function parseEvent(value: unknown): LiveSummaryStateEvent | null {
  if (!isRecord(value) || value.schemaVersion !== SUMMARY_SCHEMA_VERSION) return null;
  if (typeof value.kind !== 'string' || !SUMMARY_EVENT_KINDS.has(value.kind as LiveSummaryStateEvent['kind'])) {
    return null;
  }
  const snapshot = parseSnapshot(value.snapshot);
  if (!snapshot) return null;
  return {
    schemaVersion: SUMMARY_SCHEMA_VERSION,
    kind: value.kind as LiveSummaryStateEvent['kind'],
    snapshot,
  };
}

function compareSnapshotOrder(left: LiveSummarySnapshot, right: LiveSummarySnapshot): number {
  const leftOrder = [left.generation, left.summaryRevision, left.transcriptCursor];
  const rightOrder = [right.generation, right.summaryRevision, right.transcriptCursor];
  for (let index = 0; index < leftOrder.length; index += 1) {
    if (leftOrder[index] !== rightOrder[index]) return leftOrder[index] - rightOrder[index];
  }
  return 0;
}

export function createLiveSummaryOverlayState(): LiveSummaryOverlayState {
  return { snapshot: null, eventKind: null };
}

export function reduceLiveSummaryOverlayEvent(
  state: LiveSummaryOverlayState,
  candidate: unknown,
): LiveSummaryOverlayReduction {
  const event = parseEvent(candidate);
  if (!event) return { state, outcome: 'invalid' };
  const current = state.snapshot;

  if (!current) {
    // A recording scope must be established by the native start boundary (or
    // an explicitly synthesized hydration boundary). Late commits from a
    // previous recording are never allowed to claim an empty overlay.
    if (event.kind !== 'session_started') {
      return { state, outcome: 'scope_mismatch' };
    }
    return { state: { snapshot: event.snapshot, eventKind: event.kind }, outcome: 'applied' };
  }
  if (!sameScope(current.scope, event.snapshot.scope)) {
    if (event.kind !== 'session_started') return { state, outcome: 'scope_mismatch' };
    return { state: { snapshot: event.snapshot, eventKind: event.kind }, outcome: 'applied' };
  }

  const order = compareSnapshotOrder(event.snapshot, current);
  if (order < 0) return { state, outcome: 'stale' };
  if (order === 0) {
    const currentPriority = state.eventKind ? EVENT_PRIORITY[state.eventKind] : -1;
    const nextPriority = EVENT_PRIORITY[event.kind];
    if (nextPriority < currentPriority) return { state, outcome: 'stale' };
    if (nextPriority === currentPriority) return { state, outcome: 'duplicate' };
  }
  return { state: { snapshot: event.snapshot, eventKind: event.kind }, outcome: 'applied' };
}

export function bindLiveSummarySnapshot(
  state: LiveSummaryOverlayState,
  transcriptScope?: string | null,
): LiveSummaryBinding {
  const normalizedScope = transcriptScope?.trim();
  if (!normalizedScope) return { status: 'missing_transcript_scope' };
  if (!state.snapshot) return { status: 'waiting_for_summary' };
  if (state.snapshot.scope.id !== normalizedScope) return { status: 'scope_mismatch' };
  return { status: 'bound', snapshot: state.snapshot };
}

export function selectAssistantItems(
  snapshot: LiveSummarySnapshot,
  mode: MeetingAssistantMode,
): LiveSummaryItem[] {
  if (mode === 'captions') return [];
  const allowedKinds: ReadonlySet<LiveSummaryItemKind> = mode === 'actions'
    ? new Set(['action_item'])
    : new Set(['topic', 'decision', 'risk', 'open_question']);
  return snapshot.items.filter((item) => (
    item.status !== 'retracted' && allowedKinds.has(item.kind)
  ));
}

export function countAssistantEvidence(items: readonly LiveSummaryItem[]): number {
  const evidenceIds = new Set<string>();
  items.forEach((item) => {
    if (item.status === 'retracted') return;
    item.evidence.forEach((evidence) => {
      evidenceIds.add(`${evidence.utterance_id}:${evidence.source_revision}:${evidence.source_event_id}`);
    });
  });
  return evidenceIds.size;
}
