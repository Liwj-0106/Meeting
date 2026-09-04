'use client';

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Radio, Settings2, Trash2, X } from 'lucide-react';
import {
  BilingualCaptionLines,
  type BilingualCaptionLine,
} from '@/components/BilingualCaptionLines';
import { MeetingAssistantModeSwitch } from '@/components/MeetingAssistantModeSwitch';
import { MeetingAssistantPanel } from '@/components/MeetingAssistantPanel';
import { useCaptionOverlaySettings } from '@/hooks/useCaptionOverlaySettings';
import {
  bindLiveSummarySnapshot,
  createLiveSummaryOverlayState,
  reduceLiveSummaryOverlayEvent,
  type LiveSummaryOverlayState,
  type MeetingAssistantMode,
} from '@/lib/live-summary-overlay';
import {
  mergeTranscriptRevisionHistories,
  upsertTranscriptRevisionHistory,
  upsertTranscripts,
} from '@/lib/transcript-events';
import {
  createTranslationReducerState,
  reduceTranslationEvent,
  registerTranslationSource,
  type TranslationEvent,
} from '@/lib/translation-events';
import {
  liveTranslationService,
  type LiveTranslationAccepted,
  type LiveTranslationSettingsView,
} from '@/services/liveTranslationService';
import {
  liveSummaryService,
  type LiveSummaryStateEvent,
} from '@/services/liveSummaryService';
import type { TranscriptHistoryItem } from '@/services/transcriptService';
import type { Transcript, TranscriptUpdate } from '@/types';

interface BackendRecordingState {
  is_recording: boolean;
  is_paused: boolean;
}

interface CaptionOverlayVisibility {
  visible: boolean;
}

type CaptionStatus = 'idle' | 'listening' | 'paused' | 'error';

interface TranslationDisplay {
  sourceEventId: string;
  state: 'pending' | 'ready' | 'error';
  text?: string;
}

const STATUS_LABELS: Record<CaptionStatus, string> = {
  idle: '实时字幕已就绪',
  listening: '正在监听',
  paused: '已暂停',
  error: '转写出错',
};

export default function CaptionOverlayPage() {
  const captionSettings = useCaptionOverlaySettings();
  const [lines, setLines] = useState<Transcript[]>([]);
  const [status, setStatus] = useState<CaptionStatus>('idle');
  const [overlayVisible, setOverlayVisible] = useState(false);
  const [translationSettings, setTranslationSettings] = useState<LiveTranslationSettingsView | null>(null);
  const [translationDisplays, setTranslationDisplays] = useState<Record<string, TranslationDisplay>>({});
  const [translationQueueRevision, setTranslationQueueRevision] = useState(0);
  const [summaryOverlayState, setSummaryOverlayState] = useState<LiveSummaryOverlayState>(
    createLiveSummaryOverlayState,
  );
  const isPausedRef = useRef(false);
  const lifecycleRevisionRef = useRef(0);
  const snapshotRequestRef = useRef(0);
  const transcriptEventsRef = useRef<TranscriptUpdate[]>([]);
  const translationReducerRef = useRef(createTranslationReducerState());
  const queuedTranslationEventsRef = useRef(new Set<string>());
  const latestSourceEventsRef = useRef(new Map<string, string>());
  const bufferedTranslationEventsRef = useRef(new Map<string, TranslationEvent[]>());
  const overlayMountedRef = useRef(true);

  const setCaptionStatus = useCallback((nextStatus: CaptionStatus) => {
    setStatus(nextStatus);
  }, []);

  useEffect(() => {
    overlayMountedRef.current = true;
    return () => {
      overlayMountedRef.current = false;
    };
  }, []);

  const resetTranslations = useCallback(() => {
    translationReducerRef.current = createTranslationReducerState();
    queuedTranslationEventsRef.current.clear();
    latestSourceEventsRef.current.clear();
    bufferedTranslationEventsRef.current.clear();
    setTranslationDisplays({});
    setTranslationQueueRevision((revision) => revision + 1);
  }, []);

  const resetSummaryOverlay = useCallback(() => {
    setSummaryOverlayState(createLiveSummaryOverlayState());
  }, []);

  const handleSummaryState = useCallback((summaryEvent: LiveSummaryStateEvent) => {
    setSummaryOverlayState((current) => {
      const reduction = reduceLiveSummaryOverlayEvent(current, summaryEvent);
      return reduction.state;
    });
  }, []);

  const applyTranslationEvent = useCallback((translationEvent: TranslationEvent) => {
    const result = reduceTranslationEvent(translationReducerRef.current, translationEvent);
    translationReducerRef.current = result.state;
    if (result.outcome === 'unbound') {
      const buffer = bufferedTranslationEventsRef.current;
      if (!buffer.has(translationEvent.source.event_id) && buffer.size >= 64) {
        const oldestKey = buffer.keys().next().value as string | undefined;
        if (oldestKey) buffer.delete(oldestKey);
      }
      const pending = buffer.get(translationEvent.source.event_id) ?? [];
      if (pending.length < 4) {
        buffer.set(translationEvent.source.event_id, [...pending, translationEvent]);
      }
      return;
    }
    if (result.outcome !== 'applied') return;
    const utteranceId = translationEvent.source.utterance_id;
    if (latestSourceEventsRef.current.get(utteranceId) !== translationEvent.source.event_id) {
      return;
    }
    if (translationEvent.status === 'retracted') {
      setTranslationDisplays((current) => {
        const next = { ...current };
        delete next[utteranceId];
        return next;
      });
      return;
    }
    if (translationEvent.status === 'failed') {
      setTranslationDisplays((current) => ({
        ...current,
        [utteranceId]: {
          sourceEventId: translationEvent.source.event_id,
          state: 'error',
        },
      }));
      return;
    }
    setTranslationDisplays((current) => ({
      ...current,
      [utteranceId]: {
        sourceEventId: translationEvent.source.event_id,
        state: 'ready',
        text: translationEvent.translated_text,
      },
    }));
  }, []);

  const registerAcceptedSource = useCallback((accepted: LiveTranslationAccepted) => {
    const utteranceId = accepted.source.utterance_id;
    if (latestSourceEventsRef.current.get(utteranceId) !== accepted.source.event_id) return;
    const result = registerTranslationSource(translationReducerRef.current, accepted.source);
    translationReducerRef.current = result.state;
    if (result.outcome !== 'applied' && result.outcome !== 'duplicate') return;
    const buffered = bufferedTranslationEventsRef.current.get(accepted.source.event_id) ?? [];
    bufferedTranslationEventsRef.current.delete(accepted.source.event_id);
    buffered.forEach(applyTranslationEvent);
  }, [applyTranslationEvent]);

  const mergeUpdates = useCallback((incoming: TranscriptUpdate[]) => {
    const mergedHistory = mergeTranscriptRevisionHistories(
      incoming,
      transcriptEventsRef.current,
    );
    transcriptEventsRef.current = mergedHistory;
    setLines(upsertTranscripts([], mergedHistory)
      .filter((line) => line.text.trim())
      .slice(-2));
  }, []);

  const handleTranscriptUpdate = useCallback((update: TranscriptUpdate) => {
    const previousHistory = transcriptEventsRef.current;
    const nextHistory = upsertTranscriptRevisionHistory(previousHistory, update);
    if (nextHistory === previousHistory) return;
    transcriptEventsRef.current = nextHistory;
    setLines(upsertTranscripts([], nextHistory)
      .filter((line) => line.text.trim())
      .slice(-2));
    if (!isPausedRef.current) {
      setCaptionStatus('listening');
    }
  }, [setCaptionStatus]);

  useEffect(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];

    const addListener = async <T,>(
      eventName: string,
      callback: (payload: T) => void,
    ) => {
      const unlisten = await listen<T>(eventName, (event) => callback(event.payload));
      if (disposed) {
        unlisten();
      } else {
        unlisteners.push(unlisten);
      }
    };

    const applyLifecycleStatus = (
      nextStatus: CaptionStatus,
      clearLines = false,
      isPaused?: boolean,
    ) => {
      lifecycleRevisionRef.current += 1;
      if (isPaused !== undefined) isPausedRef.current = isPaused;
      if (clearLines) setLines([]);
      setCaptionStatus(nextStatus);
    };

    const synchronizeWithBackend = async () => {
      const requestId = ++snapshotRequestRef.current;
      const lifecycleRevision = lifecycleRevisionRef.current;

      try {
        const [recordingState, history] = await Promise.all([
          invoke<BackendRecordingState>('get_recording_state'),
          invoke<TranscriptHistoryItem[]>('get_transcript_history'),
        ]);

        if (
          disposed ||
          requestId !== snapshotRequestRef.current ||
          lifecycleRevision !== lifecycleRevisionRef.current
        ) {
          return;
        }

        isPausedRef.current = recordingState.is_paused;
        setCaptionStatus(
          recordingState.is_paused
            ? 'paused'
            : recordingState.is_recording
              ? 'listening'
              : 'idle',
        );
        mergeUpdates(
          history.map((item) => ({
            ...item,
            text: item.text,
            timestamp: item.timestamp ?? item.display_time,
            source: item.source ?? item.audio_source ?? 'Audio',
            sequence_id: item.sequence_id,
            chunk_start_time: item.chunk_start_time ?? item.audio_start_time ?? 0,
            is_partial: item.is_partial ?? false,
            confidence: item.confidence ?? 0,
            audio_start_time: item.audio_start_time ?? 0,
            audio_end_time: item.audio_end_time ?? item.audio_start_time ?? 0,
            duration: item.duration ?? 0,
          })),
        );
      } catch (error) {
        console.error('Failed to synchronize caption overlay:', error);
        if (
          !disposed &&
          requestId === snapshotRequestRef.current &&
          lifecycleRevision === lifecycleRevisionRef.current
        ) {
          setCaptionStatus('error');
        }
      }
    };

    const initialize = async () => {
      try {
        await Promise.all([
          addListener<TranscriptUpdate>('transcript-update', handleTranscriptUpdate),
          addListener('recording-started', () => {
            transcriptEventsRef.current = [];
            resetTranslations();
            resetSummaryOverlay();
            applyLifecycleStatus('listening', true, false);
          }),
          addListener('recording-paused', () => applyLifecycleStatus('paused', false, true)),
          addListener('recording-resumed', () => applyLifecycleStatus('listening', false, false)),
          addListener('recording-stopped', () => applyLifecycleStatus('idle', false, false)),
          addListener('transcription-error', () => applyLifecycleStatus('error')),
          addListener<CaptionOverlayVisibility>(
            'caption-overlay-visibility-changed',
            (visibility) => {
              setOverlayVisible(visibility.visible);
              if (visibility.visible) synchronizeWithBackend();
            },
          ),
          addListener<LiveTranslationSettingsView>(
            'live-translation-settings-changed',
            (nextSettings) => {
              setTranslationSettings(nextSettings);
              resetTranslations();
            },
          ),
          addListener<TranslationEvent>('live-translation-update', applyTranslationEvent),
          (async () => {
            const unlisten = await liveSummaryService.onState(handleSummaryState);
            if (disposed) unlisten();
            else unlisteners.push(unlisten);
          })(),
        ]);

        try {
          const [nextTranslationSettings, isOverlayVisible] = await Promise.all([
            liveTranslationService.getSettings(),
            invoke<boolean>('is_caption_overlay_visible'),
          ]);
          if (!disposed) {
            setTranslationSettings(nextTranslationSettings);
            setOverlayVisible(isOverlayVisible);
          }
        } catch {
          if (!disposed) setTranslationSettings(null);
        }

        // Summary recovery is deliberately isolated from caption and
        // translation initialization. A disabled provider or unavailable
        // registry must never make the overlay itself unusable.
        try {
          const recordingSummaryScopes = await liveSummaryService.listRecordingScopes();
          if (!disposed) {
            const activeScopes = recordingSummaryScopes.filter((entry) => (
              entry.state === 'active'
              && entry.summary.scope.kind === 'recording_session'
              && entry.summary.sessionScopeId === entry.summary.scope.id
            ));
            if (activeScopes.length === 1) {
              handleSummaryState({
                schemaVersion: 1,
                kind: 'session_started',
                snapshot: activeScopes[0].summary,
              });
            }
          }
        } catch (error) {
          console.warn('Live summary session hydration is unavailable', {
            code: typeof error === 'object' && error && 'code' in error
              ? String(error.code)
              : 'unavailable',
          });
        }
        await synchronizeWithBackend();
      } catch (error) {
        console.error('Failed to initialize caption overlay:', error);
        if (!disposed) setCaptionStatus('error');
      }
    };

    initialize();

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        invoke('set_caption_overlay_visible', { visible: false }).catch(console.error);
        return;
      }
      if (!event.altKey || event.ctrlKey || event.metaKey) return;
      const shortcutModes: Record<string, MeetingAssistantMode> = {
        '1': 'captions',
        '2': 'highlights',
        '3': 'actions',
      };
      const nextMode = shortcutModes[event.key];
      if (nextMode) {
        event.preventDefault();
        captionSettings.updateSettings({ assistant_mode: nextMode }, { immediate: true });
      }
    };
    const handleVisibilityChange = () => {
      if (document.visibilityState === 'visible') {
        synchronizeWithBackend();
      }
    };
    window.addEventListener('keydown', handleKeyDown);
    document.addEventListener('visibilitychange', handleVisibilityChange);

    return () => {
      disposed = true;
      snapshotRequestRef.current += 1;
      unlisteners.forEach((unlisten) => unlisten());
      window.removeEventListener('keydown', handleKeyDown);
      document.removeEventListener('visibilitychange', handleVisibilityChange);
    };
  }, [
    applyTranslationEvent,
    handleTranscriptUpdate,
    mergeUpdates,
    resetTranslations,
    resetSummaryOverlay,
    handleSummaryState,
    captionSettings.updateSettings,
    setCaptionStatus,
  ]);

  useEffect(() => {
    if (!translationSettings?.enabled || !overlayVisible) return;
    if (queuedTranslationEventsRef.current.size >= 512) {
      queuedTranslationEventsRef.current.clear();
    }

    lines.forEach((line) => {
      const eventId = line.event_id;
      const utteranceId = line.utterance_id;
      if (!eventId || !utteranceId || queuedTranslationEventsRef.current.has(eventId)) return;
      queuedTranslationEventsRef.current.add(eventId);
      latestSourceEventsRef.current.set(utteranceId, eventId);
      setTranslationDisplays((current) => ({
        ...current,
        [utteranceId]: { sourceEventId: eventId, state: 'pending' },
      }));

      void liveTranslationService.queue(eventId)
        .then((accepted) => {
          if (overlayMountedRef.current) registerAcceptedSource(accepted);
        })
        .catch((error: unknown) => {
          if (
            !overlayMountedRef.current
            || latestSourceEventsRef.current.get(utteranceId) !== eventId
          ) return;
          const safeError = liveTranslationService.toFrontendError(error);
          if (
            safeError.code === 'translation_disabled'
            || safeError.code === 'translation_overlay_hidden'
            || safeError.code === 'translation_source_stale'
          ) {
            return;
          }
          if (
            safeError.retryable
            && (
              safeError.code === 'translation_queue_full'
              || safeError.code === 'translation_source_not_ready'
            )
          ) {
            queuedTranslationEventsRef.current.delete(eventId);
            window.setTimeout(() => {
              if (
                overlayMountedRef.current
                && latestSourceEventsRef.current.get(utteranceId) === eventId
              ) {
                setTranslationQueueRevision((revision) => revision + 1);
              }
            }, 750);
            return;
          }
          setTranslationDisplays((current) => ({
            ...current,
            [utteranceId]: {
              sourceEventId: eventId,
              state: 'error',
            },
          }));
        });
    });
  }, [
    lines,
    overlayVisible,
    registerAcceptedSource,
    translationQueueRevision,
    translationSettings,
  ]);

  const hasCaptions = lines.length > 0;
  const bilingualLines = useMemo<BilingualCaptionLine[]>(() => lines.map((line) => {
    const utteranceId = line.utterance_id;
    const display = utteranceId ? translationDisplays[utteranceId] : undefined;
    const currentDisplay = display && display.sourceEventId === line.event_id ? display : undefined;
    return {
      id: line.utterance_id ?? line.id,
      original: line.text,
      translation: currentDisplay?.text,
      translationState: currentDisplay?.state ?? 'idle',
    };
  }), [lines, translationDisplays]);
  const { settings } = captionSettings;
  const currentTranscriptScope = useMemo(() => {
    for (let index = lines.length - 1; index >= 0; index -= 1) {
      const meetingScope = lines[index].meeting_id?.trim();
      if (meetingScope) return meetingScope;
    }
    const summaryScope = summaryOverlayState.snapshot?.scope;
    if (
      (status === 'listening' || status === 'paused')
      && summaryScope?.kind === 'recording_session'
      && summaryOverlayState.snapshot?.sessionScopeId === summaryScope.id
    ) {
      return summaryScope.id;
    }
    return null;
  }, [lines, status, summaryOverlayState.snapshot]);
  const summaryBinding = useMemo(
    () => bindLiveSummarySnapshot(summaryOverlayState, currentTranscriptScope),
    [currentTranscriptScope, summaryOverlayState],
  );
  const statusColor = useMemo(() => {
    if (status === 'error') return 'bg-red-400';
    if (status === 'paused') return 'bg-amber-400';
    if (status === 'listening') return 'bg-emerald-400';
    return 'bg-white/45';
  }, [status]);

  const overlayStyle = useMemo(() => {
    const alpha = settings.background_opacity / 100;
    return {
      backgroundColor: `rgba(8, 10, 14, ${alpha})`,
      borderColor: `rgba(255, 255, 255, ${Math.max(0.06, alpha * 0.18)})`,
      backdropFilter: alpha > 0.08 ? `blur(${Math.round(8 + alpha * 10)}px)` : 'none',
    };
  }, [settings.background_opacity]);

  const hideOverlay = () => {
    invoke('set_caption_overlay_visible', { visible: false }).catch(console.error);
  };

  const setAssistantMode = useCallback((mode: MeetingAssistantMode) => {
    captionSettings.updateSettings({ assistant_mode: mode }, { immediate: true });
  }, [captionSettings.updateSettings]);

  const emptyMessage = useMemo(() => {
    if (status === 'listening') return '正在等待语音…';
    if (status === 'paused') return '实时字幕已暂停';
    if (status === 'error') return '转写失败，请检查 Meetily 主窗口。';
    return '开始录制后即可显示实时字幕';
  }, [status]);

  return (
    <main className="group h-screen w-screen select-none p-2 text-white">
      <section
        className="relative flex h-full w-full flex-col overflow-hidden rounded-2xl border shadow-2xl"
        style={overlayStyle}
        aria-live="polite"
      >
        <header
          className="grid h-9 shrink-0 grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center px-3 text-xs text-white/70"
          data-tauri-drag-region
        >
          <div className="flex min-w-0 items-center gap-2" data-tauri-drag-region>
            <span className={`h-2 w-2 shrink-0 rounded-full ${statusColor}`} />
            <span className="min-w-0 truncate" data-tauri-drag-region>
              {STATUS_LABELS[status]}
              {settings.mouse_passthrough ? (
                <span className="hidden sm:inline"> · 鼠标穿透已开启</span>
              ) : null}
            </span>
          </div>

          <MeetingAssistantModeSwitch
            mode={settings.assistant_mode}
            onModeChange={setAssistantMode}
          />

          <div className="flex items-center justify-self-end gap-1 opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100">
            <button
              type="button"
              onClick={() => {
                invoke('set_caption_settings_visible', { visible: true }).catch(console.error);
              }}
              className="rounded-md p-1.5 text-white/65 transition-colors hover:bg-white/10 hover:text-white"
              aria-label="会议助手设置"
              aria-haspopup="dialog"
              title="会议助手设置"
            >
              <Settings2 className="h-3.5 w-3.5" />
            </button>
            {settings.assistant_mode === 'captions' ? (
              <button
                type="button"
                onClick={() => {
                  setLines([]);
                  resetTranslations();
                }}
                className="rounded-md p-1.5 text-white/65 transition-colors hover:bg-white/10 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#7DD3FC]"
                aria-label="清空字幕"
                title="清空字幕"
              >
                <Trash2 className="h-3.5 w-3.5" />
              </button>
            ) : null}
            <button
              type="button"
              onClick={hideOverlay}
              className="rounded-md p-1.5 text-white/65 transition-colors hover:bg-white/10 hover:text-white"
              aria-label="隐藏会议助手"
              title="隐藏会议助手"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
        </header>

        {settings.assistant_mode === 'captions' ? (
          <div className="flex min-h-0 flex-1 items-center justify-center px-7 pb-5 text-center">
            {hasCaptions ? (
              <BilingualCaptionLines lines={bilingualLines} fontSize={settings.font_size} />
            ) : (
              <div
                className="flex items-center gap-2 text-white/55"
                style={{
                  fontSize: `${Math.max(14, Math.round(settings.font_size * 0.68))}px`,
                  textShadow: '0 2px 9px rgba(0, 0, 0, 0.9)',
                }}
              >
                <Radio className={status === 'listening' ? 'h-5 w-5 animate-pulse' : 'h-5 w-5'} />
                <span>{emptyMessage}</span>
              </div>
            )}
          </div>
        ) : (
          <div className="min-h-0 flex-1">
            <MeetingAssistantPanel
              mode={settings.assistant_mode}
              binding={summaryBinding}
              fontSize={settings.font_size}
            />
          </div>
        )}
      </section>
    </main>
  );
}
