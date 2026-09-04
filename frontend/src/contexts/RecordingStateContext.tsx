'use client';

import React, { createContext, useContext, useState, useEffect, useRef, useCallback, useMemo } from 'react';
import { recordingService } from '@/services/recordingService';
import {
  createInitialAudioSourceHealthState,
  deriveOverallAudioHealth,
  isFatalAudioSourceEvent,
  listAudioSourceHealth,
  reduceAudioSourceHealth,
  reduceAudioSourceHealthEvents,
  type AudioTimelineEvent,
  type OverallAudioHealth,
} from '@/lib/audio-source-health';
import {
  createInitialAsrHealthState,
  isAsrHealthDegraded,
  reduceAsrHealth,
  reduceAsrHealthEvents,
  type AsrHealthEvent,
} from '@/lib/asr-health';

/**
 * Recording state synchronized with backend
 * This context provides a single source of truth for recording state
 * that automatically syncs with the Rust backend, solving:
 * 1. Page refresh desync (backend recording but UI shows stopped)
 * 2. Pause state visibility across components
 * 3. Comprehensive state for future features (reconnection, etc.)
 */

// Recording lifecycle status enum
export enum RecordingStatus {
  IDLE = 'idle',                          // Not recording
  STARTING = 'starting',                  // Initiating recording
  RECORDING = 'recording',                // Active recording
  STOPPING = 'stopping',                  // Stop initiated, waiting for backend
  PROCESSING_TRANSCRIPTS = 'processing',  // Transcription completion wait
  SAVING = 'saving',                      // Saving to database
  COMPLETED = 'completed',                // Successfully saved
  ERROR = 'error'                         // Error occurred
}

interface RecordingState {
  isRecording: boolean;           // Is a recording session active
  isPaused: boolean;              // Is the recording paused
  isActive: boolean;              // Is actively recording (recording && !paused)
  recordingDuration: number | null;  // Total duration including pauses
  activeDuration: number | null;     // Active recording time (excluding pauses)

  // NEW: Lifecycle status
  status: RecordingStatus;
  statusMessage?: string;  // Optional message for current status
}

interface RecordingStateContextType extends RecordingState {
  // NEW: Setters for status management
  setStatus: (status: RecordingStatus, message?: string) => void;

  // Computed helpers (derived from status)
  isStopping: boolean;
  isProcessing: boolean;
  isSaving: boolean;

  // Latest event accepted independently for every capture/source track.
  sourceHealth: Readonly<Record<string, AudioTimelineEvent>>;
  overallAudioHealth: OverallAudioHealth;
  isAudioDegraded: boolean;
  audioFatalError: AudioTimelineEvent | null;

  // Cloud-ASR transport state is deliberately separate from audio capture.
  asrHealth: AsrHealthEvent | null;
  isAsrDegraded: boolean;
}

const RecordingStateContext = createContext<RecordingStateContextType | null>(null);

export const useRecordingState = () => {
  const context = useContext(RecordingStateContext);
  if (!context) {
    throw new Error('useRecordingState must be used within a RecordingStateProvider');
  }
  return context;
};

export function RecordingStateProvider({ children }: { children: React.ReactNode }) {
  const [state, setState] = useState<RecordingState>({
    isRecording: false,
    isPaused: false,
    isActive: false,
    recordingDuration: null,
    activeDuration: null,
    status: RecordingStatus.IDLE,  // NEW: Initialize with IDLE status
    statusMessage: undefined,       // NEW: No message initially
  });
  const [audioHealthState, setAudioHealthState] = useState(
    createInitialAudioSourceHealthState,
  );
  const [asrHealthState, setAsrHealthState] = useState(
    createInitialAsrHealthState,
  );

  const pollingIntervalRef = useRef<NodeJS.Timeout | null>(null);

  // NEW: Status setter with logging
  const setStatus = useCallback((status: RecordingStatus, message?: string) => {
    console.log(`[RecordingState] Status: ${state.status} → ${status}`, message || '');

    setState(prev => ({
      ...prev,
      status,
      statusMessage: message,
    }));
  }, [state.status, state.isRecording, state.isPaused]);

  /**
   * Sync recording state with backend
   * Called on mount (fixes refresh desync) and periodically while recording
   */
  const syncWithBackend = async () => {
    try {
      const backendState = await recordingService.getRecordingState();

      setState(prev => ({
        ...prev,
        isRecording: backendState.is_recording,
        isPaused: backendState.is_paused,
        isActive: backendState.is_active,
        recordingDuration: backendState.recording_duration,
        activeDuration: backendState.active_duration,
      }));

      console.log('[RecordingStateContext] Synced with backend:', backendState);
    } catch (error) {
      console.error('[RecordingStateContext] Failed to sync with backend:', error);
      // Don't update state on error - keep current state
    }
  };

  /**
   * Start polling backend state (called when recording starts)
   */
  const startPolling = () => {
    if (pollingIntervalRef.current) {
      clearInterval(pollingIntervalRef.current);
    }

    console.log('[RecordingStateContext] Starting state polling (500ms interval)');
    pollingIntervalRef.current = setInterval(syncWithBackend, 500);
  };

  /**
   * Stop polling backend state (called when recording stops)
   */
  const stopPolling = () => {
    if (pollingIntervalRef.current) {
      console.log('[RecordingStateContext] Stopping state polling');
      clearInterval(pollingIntervalRef.current);
      pollingIntervalRef.current = null;
    }
  };

  /**
   * Set up event listeners for backend state changes
   */
  useEffect(() => {
    console.log('[RecordingStateContext] Setting up event listeners');
    const unsubscribers: (() => void)[] = [];
    const bufferedAudioHealthEvents: AudioTimelineEvent[] = [];
    const bufferedAsrHealthEvents: AsrHealthEvent[] = [];
    let audioHealthSnapshotPending = true;
    let asrHealthSnapshotPending = true;
    let disposed = false;

    const retainUnsubscriber = (unlisten: () => void) => {
      if (disposed) {
        unlisten();
      } else {
        unsubscribers.push(unlisten);
      }
    };

    const acceptAudioHealthEvent = (event: AudioTimelineEvent) => {
      if (audioHealthSnapshotPending) {
        bufferedAudioHealthEvents.push(event);
      } else {
        setAudioHealthState(previous => reduceAudioSourceHealth(previous, event));
      }

      if (isFatalAudioSourceEvent(event)) {
        console.error('[RecordingStateContext] Fatal audio source error:', {
          track: event.track,
          code: event.code,
          detail: event.detail,
        });
      }
    };

    const finishAudioHealthHydration = (snapshot: readonly AudioTimelineEvent[]) => {
      const buffered = bufferedAudioHealthEvents.splice(0);
      audioHealthSnapshotPending = false;
      if (disposed) {
        return;
      }

      // The listener was installed first. Snapshot events establish the
      // persisted prefix and the buffered live tail closes the command race;
      // duplicate IDs are ignored by the shared reducer.
      setAudioHealthState(previous => reduceAudioSourceHealthEvents(
        previous,
        [...snapshot, ...buffered],
      ));
    };

    const acceptAsrHealthEvent = (event: AsrHealthEvent) => {
      if (asrHealthSnapshotPending) {
        bufferedAsrHealthEvents.push(event);
      } else {
        setAsrHealthState(previous => reduceAsrHealth(previous, event));
      }
    };

    const finishAsrHealthHydration = (snapshot: readonly AsrHealthEvent[]) => {
      const buffered = bufferedAsrHealthEvents.splice(0);
      asrHealthSnapshotPending = false;
      if (disposed) return;
      setAsrHealthState(previous => reduceAsrHealthEvents(
        previous,
        [...snapshot, ...buffered],
      ));
    };

    const setupListeners = async () => {
      let audioHealthSnapshot: Promise<AudioTimelineEvent[]> | null = null;
      let asrHealthSnapshot: Promise<AsrHealthEvent[]> | null = null;

      try {
        // Subscribe before requesting the snapshot. Events emitted while the
        // command is in flight are buffered and replayed after its persisted
        // prefix, preventing a WebView reload from losing a transition.
        const unlistenSourceHealth = await recordingService.onAudioSourceHealth(
          acceptAudioHealthEvent,
        );
        retainUnsubscriber(unlistenSourceHealth);
        audioHealthSnapshot = recordingService.getAudioSourceHealthSnapshot();

        const unlistenAsrHealth = await recordingService.onAsrHealth(
          acceptAsrHealthEvent,
        );
        retainUnsubscriber(unlistenAsrHealth);
        asrHealthSnapshot = recordingService.getAsrHealthSnapshot();

        // Recording started
        const unlistenStarted = await recordingService.onRecordingStarted(() => {
          console.log('[RecordingStateContext] Recording started event');
          setAudioHealthState(createInitialAudioSourceHealthState());
          setAsrHealthState(createInitialAsrHealthState());
          setState(prev => ({
            ...prev,
            isRecording: true,
            isPaused: false,
            isActive: true,
            status: RecordingStatus.RECORDING,  // NEW: Set status to RECORDING
          }));
          startPolling();
        });
        retainUnsubscriber(unlistenStarted);

        // Recording stopped
        const unlistenStopped = await recordingService.onRecordingStopped((payload) => {
          console.log('[RecordingStateContext] Recording stopped event:', payload);
          setAudioHealthState(createInitialAudioSourceHealthState());
          setAsrHealthState(createInitialAsrHealthState());
          setState(prev => {
            // Set status to STOPPING if not already in stop flow
            // This ensures smooth UI transition for tray/keyboard stops
            const newStatus = [
              RecordingStatus.STOPPING,
              RecordingStatus.PROCESSING_TRANSCRIPTS,
              RecordingStatus.SAVING
            ].includes(prev.status)
              ? prev.status  // Already in stop flow
              : RecordingStatus.STOPPING;  // New stop, transition smoothly

            return {
              ...prev,
              status: newStatus,
              statusMessage: newStatus === RecordingStatus.STOPPING ? 'Stopping recording...' : prev.statusMessage,
              isRecording: false,
              isPaused: false,
              isActive: false,
              recordingDuration: null,
              activeDuration: null,
            };
          });
          stopPolling();
        });
        retainUnsubscriber(unlistenStopped);

        // Recording paused
        const unlistenPaused = await recordingService.onRecordingPaused(() => {
          console.log('[RecordingStateContext] Recording paused event');
          setState(prev => ({
            ...prev,
            isPaused: true,
            isActive: false,
          }));
        });
        retainUnsubscriber(unlistenPaused);

        // Recording resumed
        const unlistenResumed = await recordingService.onRecordingResumed(() => {
          console.log('[RecordingStateContext] Recording resumed event');
          setState(prev => ({
            ...prev,
            isPaused: false,
            isActive: true,
          }));
        });
        retainUnsubscriber(unlistenResumed);

        console.log('[RecordingStateContext] Event listeners set up successfully');
      } catch (error) {
        console.error('[RecordingStateContext] Failed to set up event listeners:', error);
      } finally {
        if (audioHealthSnapshot) {
          try {
            finishAudioHealthHydration(await audioHealthSnapshot);
          } catch (error) {
            console.error(
              '[RecordingStateContext] Failed to restore audio source health snapshot:',
              error,
            );
            // Live events remain authoritative even if the read-only recovery
            // command fails, so release the buffer instead of leaving it stuck.
            finishAudioHealthHydration([]);
          }
        } else {
          audioHealthSnapshotPending = false;
        }

        if (asrHealthSnapshot) {
          try {
            finishAsrHealthHydration(await asrHealthSnapshot);
          } catch (error) {
            console.error(
              '[RecordingStateContext] Failed to restore ASR health snapshot:',
              error,
            );
            finishAsrHealthHydration([]);
          }
        } else {
          asrHealthSnapshotPending = false;
        }
      }
    };

    setupListeners();

    return () => {
      console.log('[RecordingStateContext] Cleaning up event listeners');
      disposed = true;
      unsubscribers.forEach(unsub => unsub());
      stopPolling();
    };
  }, []);

  /**
   * Initial sync on mount - CRITICAL for fixing refresh desync bug
   * If backend is recording but UI state is false, this will correct it
   */
  useEffect(() => {
    console.log('[RecordingStateContext] Initial mount - syncing with backend');
    syncWithBackend();
  }, []);

  const overallAudioHealth = useMemo(
    () => deriveOverallAudioHealth(audioHealthState.byTrack),
    [audioHealthState.byTrack],
  );

  const audioFatalError = useMemo(
    () => listAudioSourceHealth(audioHealthState.byTrack)
      .filter(isFatalAudioSourceEvent)
      .sort((left, right) => right.sequence - left.sequence)[0] ?? null,
    [audioHealthState.byTrack],
  );

  const asrHealth = asrHealthState.latest;
  const isAsrDegraded = isAsrHealthDegraded(asrHealth);

  // NEW: Computed helpers from status
  const contextValue = useMemo(() => ({
    ...state,
    setStatus,
    isStopping: state.status === RecordingStatus.STOPPING,
    isProcessing: state.status === RecordingStatus.PROCESSING_TRANSCRIPTS,
    isSaving: state.status === RecordingStatus.SAVING,
    sourceHealth: audioHealthState.byTrack,
    overallAudioHealth,
    isAudioDegraded: overallAudioHealth === 'degraded' || overallAudioHealth === 'fatal',
    audioFatalError,
    asrHealth,
    isAsrDegraded,
  }), [
    state,
    setStatus,
    audioHealthState.byTrack,
    overallAudioHealth,
    audioFatalError,
    asrHealth,
    isAsrDegraded,
  ]);

  return (
    <RecordingStateContext.Provider value={contextValue}>
      {children}
    </RecordingStateContext.Provider>
  );
}
