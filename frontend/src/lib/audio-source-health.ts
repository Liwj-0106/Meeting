export type ForwardCompatibleString<Known extends string> =
  | Known
  | (string & {});

export type AudioTrack = ForwardCompatibleString<
  'microphone' | 'system' | 'mixed' | 'imported'
>;

export type AudioTimelineEventKind = ForwardCompatibleString<
  | 'session_started'
  | 'session_stopped'
  | 'track_configured'
  | 'state_changed'
  | 'device_changed'
  | 'device_lost'
  | 'device_recovered'
  | 'stream_interrupted'
  | 'stream_restart_scheduled'
  | 'stream_restarted'
  | 'clock_rebased'
  | 'gap_detected'
  | 'permission_denied'
  | 'fatal_error'
>;

export type AudioTrackState = ForwardCompatibleString<
  | 'unconfigured'
  | 'ready'
  | 'starting'
  | 'healthy'
  | 'degraded'
  | 'interrupted'
  | 'recovering'
  | 'stopped'
  | 'failed'
>;

export type AudioEventSeverity = ForwardCompatibleString<
  'trace' | 'info' | 'warning' | 'error' | 'fatal'
>;

/**
 * Wire payload emitted by Rust as `audio-source-health`.
 *
 * Enum-like fields intentionally accept unknown strings so a newer backend can
 * add tracks or states without making an older frontend discard the event.
 */
export interface AudioTimelineEvent {
  schema_version: number;
  event_id: string;
  session_id: string;
  sequence: number;
  track: AudioTrack;
  kind: AudioTimelineEventKind;
  state: AudioTrackState;
  severity: AudioEventSeverity;
  code: string;
  recoverable: boolean;
  attempt: number;
  at_frame: number;
  end_frame: number | null;
  generation: number;
  gap_frames: number;
  device_label: string | null;
  detail: string | null;
  created_at: string;
  [key: string]: unknown;
}

export interface AudioSourceHealthReducerState {
  readonly sessionId: string | null;
  readonly byTrack: Readonly<Record<string, AudioTimelineEvent>>;
  readonly seenEventIds: Readonly<Record<string, true>>;
}

export type OverallAudioHealth = 'unknown' | 'healthy' | 'degraded' | 'fatal';

export type AudioHealthTone =
  | 'healthy'
  | 'progress'
  | 'warning'
  | 'fatal'
  | 'neutral';

export interface AudioSourceHealthPresentation {
  label: string;
  tone: AudioHealthTone;
  detail?: string;
}

const TRACK_ORDER: Readonly<Record<string, number>> = {
  microphone: 0,
  system: 1,
  mixed: 2,
  imported: 3,
};

const DEGRADED_STATES = new Set<AudioTrackState>([
  'degraded',
  'interrupted',
  'recovering',
  'failed',
]);

export function createInitialAudioSourceHealthState(): AudioSourceHealthReducerState {
  return {
    sessionId: null,
    byTrack: {},
    seenEventIds: {},
  };
}

function isNonNegativeInteger(value: number): boolean {
  return Number.isSafeInteger(value) && value >= 0;
}

function isUsableEvent(event: AudioTimelineEvent): boolean {
  return Boolean(
    event &&
      typeof event.event_id === 'string' &&
      event.event_id.length > 0 &&
      typeof event.session_id === 'string' &&
      event.session_id.length > 0 &&
      typeof event.track === 'string' &&
      event.track.length > 0 &&
      typeof event.state === 'string' &&
      event.state.length > 0 &&
      isNonNegativeInteger(event.sequence) &&
      isNonNegativeInteger(event.generation),
  );
}

/**
 * Applies one health event without mutating the previous state.
 *
 * Delivery is at-least-once, so event IDs are globally idempotent. Ordering is
 * checked per track because unrelated microphone and system events can arrive
 * in separate WebView tasks. A track can move to a newer stream generation but
 * can never move back to an older generation.
 */
export function reduceAudioSourceHealth(
  previous: AudioSourceHealthReducerState,
  event: AudioTimelineEvent,
): AudioSourceHealthReducerState {
  if (!isUsableEvent(event) || previous.seenEventIds[event.event_id]) {
    return previous;
  }

  let base = previous;
  if (previous.sessionId && previous.sessionId !== event.session_id) {
    // A delayed event from an older recording cannot replace the active
    // session. Only an explicit new-session boundary is allowed to reset it.
    if (event.kind !== 'session_started') {
      return previous;
    }
    base = createInitialAudioSourceHealthState();
  }

  const current = base.byTrack[event.track];
  if (
    current &&
    (event.sequence <= current.sequence || event.generation < current.generation)
  ) {
    return previous;
  }

  return {
    sessionId: event.session_id,
    byTrack: {
      ...base.byTrack,
      [event.track]: event,
    },
    seenEventIds: {
      ...base.seenEventIds,
      [event.event_id]: true,
    },
  };
}

/** Fold an ordered snapshot or buffered live batch through the same idempotent
 * reducer used for individual Tauri events. Keeping one acceptance path avoids
 * snapshot/live disagreements during a WebView reload. */
export function reduceAudioSourceHealthEvents(
  previous: AudioSourceHealthReducerState,
  events: readonly AudioTimelineEvent[],
): AudioSourceHealthReducerState {
  return events.reduce(reduceAudioSourceHealth, previous);
}

export function isFatalAudioSourceEvent(event: AudioTimelineEvent): boolean {
  return event.severity === 'fatal' || event.kind === 'fatal_error';
}

export function isDegradedAudioSourceEvent(event: AudioTimelineEvent): boolean {
  return (
    isFatalAudioSourceEvent(event) ||
    DEGRADED_STATES.has(event.state) ||
    event.severity === 'warning' ||
    event.severity === 'error'
  );
}

export function deriveOverallAudioHealth(
  sourceHealth: Readonly<Record<string, AudioTimelineEvent>>,
): OverallAudioHealth {
  const events = Object.values(sourceHealth);
  if (events.length === 0) {
    return 'unknown';
  }
  if (events.some(isFatalAudioSourceEvent)) {
    return 'fatal';
  }
  if (events.some(isDegradedAudioSourceEvent)) {
    return 'degraded';
  }
  return 'healthy';
}

export function listAudioSourceHealth(
  sourceHealth: Readonly<Record<string, AudioTimelineEvent>>,
): AudioTimelineEvent[] {
  return Object.values(sourceHealth).sort((left, right) => {
    const leftOrder = TRACK_ORDER[left.track] ?? Number.MAX_SAFE_INTEGER;
    const rightOrder = TRACK_ORDER[right.track] ?? Number.MAX_SAFE_INTEGER;
    return leftOrder - rightOrder || left.track.localeCompare(right.track);
  });
}

function getTrackLabel(event: AudioTimelineEvent): string {
  switch (event.track) {
    case 'microphone':
      return '麦克风';
    case 'system':
      return '媒体';
    case 'mixed':
      return '混合流';
    case 'imported':
      return '导入音频';
    default:
      return event.device_label?.trim() || '其他音源';
  }
}

function getStatePresentation(event: AudioTimelineEvent): Pick<AudioSourceHealthPresentation, 'label' | 'tone'> {
  if (isFatalAudioSourceEvent(event)) {
    return { label: '严重异常', tone: 'fatal' };
  }

  switch (event.state) {
    case 'healthy':
      return { label: '正常', tone: 'healthy' };
    case 'ready':
      return { label: '就绪', tone: 'healthy' };
    case 'starting':
      return { label: '启动中', tone: 'progress' };
    case 'recovering':
      return { label: '重连中', tone: 'progress' };
    case 'degraded':
      return { label: '不稳定', tone: 'warning' };
    case 'interrupted':
      return { label: '已中断', tone: 'warning' };
    case 'failed':
      return { label: '异常', tone: 'warning' };
    case 'stopped':
      return { label: '已停止', tone: 'neutral' };
    case 'unconfigured':
      return { label: '未配置', tone: 'neutral' };
    default:
      return {
        label: event.severity === 'warning' || event.severity === 'error'
          ? '状态异常'
          : '状态已更新',
        tone: isDegradedAudioSourceEvent(event) ? 'warning' : 'neutral',
      };
  }
}

export function presentAudioSourceHealth(
  event: AudioTimelineEvent,
): AudioSourceHealthPresentation {
  const state = getStatePresentation(event);
  return {
    label: `${getTrackLabel(event)}${state.label}`,
    tone: state.tone,
    detail: event.detail?.trim() || undefined,
  };
}
