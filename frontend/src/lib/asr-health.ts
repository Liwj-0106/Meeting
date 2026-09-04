export type ForwardCompatibleAsrString<Known extends string> =
  | Known
  | (string & {});

export type AsrProvider = ForwardCompatibleAsrString<'deepgram' | 'openai_realtime'>;
export type AsrHealthEventKind = ForwardCompatibleAsrString<
  | 'session_starting'
  | 'connection_changed'
  | 'reconnect_scheduled'
  | 'replay_started'
  | 'buffer_overflow'
  | 'provider_error'
  | 'fallback_activated'
  | 'session_stopped'
>;
export type AsrHealthState = ForwardCompatibleAsrString<
  | 'idle'
  | 'connecting'
  | 'streaming'
  | 'backoff'
  | 'replaying'
  | 'degraded'
  | 'fallback'
  | 'stopped'
  | 'failed'
>;
export type AsrHealthSeverity = ForwardCompatibleAsrString<
  'info' | 'warning' | 'error' | 'fatal'
>;

export interface AsrHealthEvent {
  schema_version: number;
  event_id: string;
  session_id: string;
  sequence: number;
  provider: AsrProvider;
  kind: AsrHealthEventKind;
  state: AsrHealthState;
  severity: AsrHealthSeverity;
  code: string;
  recoverable: boolean;
  attempt: number;
  at_frame: number;
  replay_from_frame: number | null;
  dropped_frames: number;
  detail: string | null;
  created_at: string;
  [key: string]: unknown;
}

export interface AsrHealthReducerState {
  readonly sessionId: string | null;
  readonly latest: AsrHealthEvent | null;
  readonly history: readonly AsrHealthEvent[];
  readonly seenEventIds: Readonly<Record<string, true>>;
}

export type AsrHealthTone = 'healthy' | 'progress' | 'warning' | 'fatal' | 'neutral';

export interface AsrHealthPresentation {
  label: string;
  tone: AsrHealthTone;
  detail?: string;
}

export function createInitialAsrHealthState(): AsrHealthReducerState {
  return {
    sessionId: null,
    latest: null,
    history: [],
    seenEventIds: {},
  };
}

function isNonNegativeInteger(value: number): boolean {
  return Number.isSafeInteger(value) && value >= 0;
}

function isUsableEvent(event: AsrHealthEvent): boolean {
  return Boolean(
    event &&
      typeof event.event_id === 'string' &&
      event.event_id.length > 0 &&
      typeof event.session_id === 'string' &&
      event.session_id.length > 0 &&
      typeof event.provider === 'string' &&
      event.provider.length > 0 &&
      typeof event.state === 'string' &&
      event.state.length > 0 &&
      isNonNegativeInteger(event.sequence),
  );
}

/** Rust allocates one globally monotonic sequence per ASR session. */
export function reduceAsrHealth(
  previous: AsrHealthReducerState,
  event: AsrHealthEvent,
): AsrHealthReducerState {
  if (!isUsableEvent(event) || previous.seenEventIds[event.event_id]) {
    return previous;
  }

  let base = previous;
  if (previous.sessionId && previous.sessionId !== event.session_id) {
    if (event.kind !== 'session_starting') return previous;
    base = createInitialAsrHealthState();
  }
  if (base.latest && event.sequence <= base.latest.sequence) {
    return previous;
  }

  return {
    sessionId: event.session_id,
    latest: event,
    history: [...base.history, event],
    seenEventIds: {
      ...base.seenEventIds,
      [event.event_id]: true,
    },
  };
}

export function reduceAsrHealthEvents(
  previous: AsrHealthReducerState,
  events: readonly AsrHealthEvent[],
): AsrHealthReducerState {
  return events.reduce(reduceAsrHealth, previous);
}

export function isAsrHealthDegraded(event: AsrHealthEvent | null): boolean {
  if (!event) return false;
  return event.severity === 'warning' ||
    event.severity === 'error' ||
    event.severity === 'fatal' ||
    event.state === 'degraded' ||
    event.state === 'fallback' ||
    event.state === 'failed';
}

export function presentAsrHealth(event: AsrHealthEvent): AsrHealthPresentation {
  const provider = event.provider === 'deepgram'
    ? 'Deepgram'
    : event.provider === 'openai_realtime'
      ? 'OpenAI'
      : '在线识别';
  const detail = event.detail?.trim() || undefined;

  if (event.severity === 'fatal' || event.state === 'failed') {
    return { label: `${provider}识别失败`, tone: 'fatal', detail };
  }
  switch (event.state) {
    case 'connecting':
      return { label: `${provider}连接中`, tone: 'progress', detail };
    case 'streaming':
      return { label: `${provider}实时识别`, tone: 'healthy', detail };
    case 'backoff':
      return { label: `${provider}等待重连`, tone: 'warning', detail };
    case 'replaying':
      return { label: `${provider}补传音频`, tone: 'progress', detail };
    case 'degraded':
      return { label: `${provider}识别不稳定`, tone: 'warning', detail };
    case 'fallback':
      return { label: '已切换本地识别', tone: 'warning', detail };
    case 'stopped':
      return { label: `${provider}已停止`, tone: 'neutral', detail };
    case 'idle':
      return { label: `${provider}待命`, tone: 'neutral', detail };
    default:
      return {
        label: `${provider}状态已更新`,
        tone: isAsrHealthDegraded(event) ? 'warning' : 'neutral',
        detail,
      };
  }
}
