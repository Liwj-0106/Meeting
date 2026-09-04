/**
 * Recording Service
 *
 * Handles all recording lifecycle Tauri backend calls and events.
 * Pure 1-to-1 wrapper - no error handling changes, exact same behavior as direct invoke/listen calls.
 */

import { invoke } from '@tauri-apps/api/core';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import type { AudioTimelineEvent } from '@/lib/audio-source-health';
import type { AsrHealthEvent } from '@/lib/asr-health';
import type { RecordingSummaryBindingTicket } from '@/services/liveSummaryService';

export type {
  AudioEventSeverity,
  AudioTimelineEvent,
  AudioTimelineEventKind,
  AudioTrack,
  AudioTrackState,
} from '@/lib/audio-source-health';
export type {
  AsrHealthEvent,
  AsrHealthEventKind,
  AsrHealthSeverity,
  AsrHealthState,
  AsrProvider,
} from '@/lib/asr-health';

export interface RecordingState {
  is_recording: boolean;
  is_paused: boolean;
  is_active: boolean;
  recording_duration: number | null;
  active_duration: number | null;
}

export interface RecordingStoppedPayload {
  message: string;
  folder_path?: string;
  meeting_name?: string;
}

export interface RecordingStartResult {
  summaryBindingTicket?: RecordingSummaryBindingTicket;
}

/**
 * Recording Service
 * Singleton service for managing recording lifecycle operations
 */
export class RecordingService {
  /**
   * Check if recording is currently active
   * @returns Promise<boolean>
   */
  async isRecording(): Promise<boolean> {
    return invoke<boolean>('is_recording');
  }

  /**
   * Get comprehensive recording state (includes durations)
   * @returns Promise with full recording state
   */
  async getRecordingState(): Promise<RecordingState> {
    return invoke<RecordingState>('get_recording_state');
  }

  /**
   * Get the complete ordered health timeline for the active recording.
   * Returns an empty list when there is no active audio timeline.
   */
  async getAudioSourceHealthSnapshot(): Promise<AudioTimelineEvent[]> {
    return invoke<AudioTimelineEvent[]>('get_audio_source_health_snapshot');
  }

  /** Restore the active online-ASR event prefix after a WebView reload. */
  async getAsrHealthSnapshot(): Promise<AsrHealthEvent[]> {
    return invoke<AsrHealthEvent[]>('get_streaming_asr_health_snapshot');
  }

  /**
   * Get current meeting name
   * @returns Promise<string | null>
   */
  async getRecordingMeetingName(): Promise<string | null> {
    return invoke<string | null>('get_recording_meeting_name');
  }

  /**
   * Start recording (no device configuration)
   * @returns Promise<void>
   */
  async startRecording(): Promise<RecordingStartResult> {
    return invoke<RecordingStartResult>('start_recording');
  }

  /**
   * Start recording with device configuration and meeting name
   * @param micDeviceName - Microphone device name (null for default, "disabled" for system-audio-only)
   * @param systemDeviceName - System audio device name (null for default, "disabled" for microphone-only)
   * @param meetingName - Meeting name/title
   * @returns A Rust-issued, one-time summary binding capability when available
   */
  async startRecordingWithDevices(
    micDeviceName: string | null,
    systemDeviceName: string | null,
    meetingName: string
  ): Promise<RecordingStartResult> {
    return invoke<RecordingStartResult>('start_recording_with_devices_and_meeting', {
      micDeviceName,
      systemDeviceName,
      meetingName
    });
  }

  /**
   * Stop recording and save to file
   * @param savePath - Path to save audio file
   * @returns Promise<void>
   */
  async stopRecording(savePath: string): Promise<void> {
    return invoke('stop_recording', {
      args: { save_path: savePath }
    });
  }

  /**
   * Pause active recording
   * @returns Promise<void>
   */
  async pauseRecording(): Promise<void> {
    return invoke('pause_recording');
  }

  /**
   * Resume paused recording
   * @returns Promise<void>
   */
  async resumeRecording(): Promise<void> {
    return invoke('resume_recording');
  }

  // Event Listeners

  /**
   * Listen for recording-started event
   * @param callback - Function to call when recording starts
   * @returns Promise that resolves to unlisten function
   */
  async onRecordingStarted(callback: () => void): Promise<UnlistenFn> {
    return listen('recording-started', callback);
  }

  /**
   * Listen for recording-stopped event (with metadata)
   * @param callback - Function to call when recording stops
   * @returns Promise that resolves to unlisten function
   */
  async onRecordingStopped(callback: (payload: RecordingStoppedPayload) => void): Promise<UnlistenFn> {
    return listen<RecordingStoppedPayload>('recording-stopped', (event) => {
      callback(event.payload);
    });
  }

  /**
   * Listen for recording-paused event
   * @param callback - Function to call when recording is paused
   * @returns Promise that resolves to unlisten function
   */
  async onRecordingPaused(callback: () => void): Promise<UnlistenFn> {
    return listen('recording-paused', callback);
  }

  /**
   * Listen for recording-resumed event
   * @param callback - Function to call when recording resumes
   * @returns Promise that resolves to unlisten function
   */
  async onRecordingResumed(callback: () => void): Promise<UnlistenFn> {
    return listen('recording-resumed', callback);
  }

  /**
   * Listen for low-volume microphone/system health transitions.
   * Unknown tracks and states are passed through for forward compatibility.
   */
  async onAudioSourceHealth(
    callback: (payload: AudioTimelineEvent) => void
  ): Promise<UnlistenFn> {
    return listen<AudioTimelineEvent>('audio-source-health', (event) => {
      callback(event.payload);
    });
  }

  /** Online ASR transport health is independent from capture-device health. */
  async onAsrHealth(callback: (payload: AsrHealthEvent) => void): Promise<UnlistenFn> {
    return listen<AsrHealthEvent>('asr-health-update', (event) => {
      callback(event.payload);
    });
  }

  /**
   * Listen for chunk-drop-warning event (audio buffer overflow)
   * @param callback - Function to call when chunks are dropped
   * @returns Promise that resolves to unlisten function
   */
  async onChunkDropWarning(callback: (warning: string) => void): Promise<UnlistenFn> {
    return listen<string>('chunk-drop-warning', (event) => {
      callback(event.payload);
    });
  }

  /**
   * Listen for speech-detected event (VAD)
   * @param callback - Function to call when speech is detected
   * @returns Promise that resolves to unlisten function
   */
  async onSpeechDetected(callback: () => void): Promise<UnlistenFn> {
    return listen('speech-detected', callback);
  }
}

// Export singleton instance
export const recordingService = new RecordingService();
