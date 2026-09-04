/**
 * useTranscriptRecovery Hook
 *
 * Orchestrates transcript recovery operations for interrupted meetings.
 * Provides functionality to detect, preview, and recover meetings from IndexedDB.
 */

import { useState, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { indexedDBService, MeetingMetadata, StoredTranscript } from '@/services/indexedDBService';
import { storageService } from '@/services/storageService';
import { applyPinnedSummaryLanguageToMeeting } from '@/lib/summary-language-preferences';
import {
  mergeTranscriptRevisionHistories,
  transcriptUpdateToTranscript,
  upsertTranscripts,
} from '@/lib/transcript-events';
import type { TranscriptUpdate } from '@/types';
import {
  isAudioRecoverySafeToCleanup,
  type AudioRecoveryStatus,
  type CheckpointCleanupResult,
} from '@/lib/audio-recovery';
import { toast } from 'sonner';

interface TranscriptFileRecoveryResult {
  events: TranscriptUpdate[];
  source: 'transcript_events_ndjson' | 'transcripts_json' | string;
  warnings: Array<{
    code: string;
    line?: number;
    message: string;
  }>;
}

async function loadRecoveryTranscriptEvents(
  meetingId: string,
  folderPath?: string,
): Promise<TranscriptUpdate[]> {
  const indexedDbEvents = await indexedDBService.getTranscriptEventUpdates(meetingId);
  if (!folderPath) return indexedDbEvents;

  try {
    const fileResult = await invoke<TranscriptFileRecoveryResult>(
      'read_recording_transcript_recovery',
      { meetingFolder: folderPath },
    );

    fileResult.warnings.forEach((warning) => {
      const line = warning.line === undefined ? '' : ` line ${warning.line}`;
      console.warn(
        `Transcript recovery (${fileResult.source}, ${warning.code}${line}): ${warning.message}`,
      );
    });

    // The native event log is the durable source of truth. IndexedDB is then
    // merged in because it can contain browser-side events that reached the UI
    // just before a native crash. The shared reducer makes the merge
    // idempotent and deterministic.
    return mergeTranscriptRevisionHistories(fileResult.events, indexedDbEvents);
  } catch (error) {
    // Older builds do not expose the file replay command. IndexedDB recovery
    // remains usable, while a new build logs the native validation/read error.
    console.warn('Native transcript event replay was unavailable:', error);
    return indexedDbEvents;
  }
}

function materializeStoredTranscripts(
  meetingId: string,
  events: readonly TranscriptUpdate[],
  storedAt: number,
): StoredTranscript[] {
  return upsertTranscripts([], events).map((transcript) => ({
    ...transcript,
    meetingId,
    sequenceId: transcript.sequence_id ?? 0,
    confidence: transcript.confidence ?? transcript.asr_confidence ?? 0,
    storedAt,
  }));
}

export interface UseTranscriptRecoveryReturn {
  recoverableMeetings: MeetingMetadata[];
  isLoading: boolean;
  isRecovering: boolean;
  checkForRecoverableTranscripts: () => Promise<void>;
  recoverMeeting: (meetingId: string) => Promise<{ success: boolean; audioRecoveryStatus?: AudioRecoveryStatus | null; meetingId?: string }>;
  loadMeetingTranscripts: (meetingId: string) => Promise<StoredTranscript[]>;
  deleteRecoverableMeeting: (meetingId: string) => Promise<void>;
}

export function useTranscriptRecovery(): UseTranscriptRecoveryReturn {
  const [recoverableMeetings, setRecoverableMeetings] = useState<MeetingMetadata[]>([]);
  const [isLoading, setIsLoading] = useState(false);
  const [isRecovering, setIsRecovering] = useState(false);

  /**
   * Check for recoverable meetings in IndexedDB
   */
  const checkForRecoverableTranscripts = useCallback(async () => {
    setIsLoading(true);
    try {
      const meetings = await indexedDBService.getAllMeetings();

      // Filter out meetings older than 7 days and newer than 15 seconds
      // The 15 seconds threshold prevents showing meetings from the current session(jus in case)
      // where recording just stopped but hasn't been fully saved yet
      const cutoffTime = Date.now() - (7 * 24 * 60 * 60 * 1000);
      const secondsAgo = Date.now() - (2 * 1000);

      const recentMeetings = meetings.filter(m => {
        const isWithinRetention = m.lastUpdated > cutoffTime; // Not older than 7 days
        const isOldEnough = m.lastUpdated < secondsAgo; // Older than 15 seconds
        return isWithinRetention && isOldEnough;
      });

      // Verify audio checkpoint availability for each meeting
      const meetingsWithAudioStatus = await Promise.all(
        recentMeetings.map(async (meeting) => {
          if (meeting.folderPath) {
            try {
              const hasAudio = await invoke<boolean>('has_audio_checkpoints', {
                meetingFolder: meeting.folderPath
              });

              // If no audio files, clear folderPath to show "No audio" in UI
              return {
                ...meeting,
                folderPath: hasAudio ? meeting.folderPath : undefined
              };
            } catch (error) {
              console.warn('Failed to check audio for meeting:', error);
              // On error, assume no audio to be safe
              return { ...meeting, folderPath: undefined };
            }
          }
          return meeting;
        })
      );


      setRecoverableMeetings(meetingsWithAudioStatus);
    } catch (error) {
      console.error('Failed to check for recoverable transcripts:', error);
      setRecoverableMeetings([]);
    } finally {
      setIsLoading(false);
    }
  }, []);

  /**
   * Load transcripts for preview
   */
  const loadMeetingTranscripts = useCallback(async (meetingId: string): Promise<StoredTranscript[]> => {
    try {
      const metadata = await indexedDBService.getMeetingMetadata(meetingId);
      const events = await loadRecoveryTranscriptEvents(meetingId, metadata?.folderPath);
      return materializeStoredTranscripts(
        meetingId,
        events,
        metadata?.lastUpdated ?? Date.now(),
      );
    } catch (error) {
      console.error('Failed to load meeting transcripts:', error);
      return [];
    }
  }, []);

  /**
   * Recover a meeting from IndexedDB
   */
  const recoverMeeting = useCallback(async (meetingId: string): Promise<{ success: boolean; audioRecoveryStatus?: AudioRecoveryStatus | null; meetingId?: string }> => {
    setIsRecovering(true);
    try {
      // 1. Load meeting metadata
      const metadata = await indexedDBService.getMeetingMetadata(meetingId);
      if (!metadata) {
        throw new Error('Meeting metadata not found');
      }

      // 2. Resolve the recording folder before loading transcripts. The native
      // NDJSON event log can contain a tail that IndexedDB missed if the
      // WebView crashed while the Rust recorder kept running.
      let folderPath = metadata.folderPath;


      if (!folderPath) {
        // Try to get from backend (might exist if only app crashed, not system)
        try {
          folderPath = await invoke<string>('get_meeting_folder_path');
        } catch (error) {
          folderPath = undefined;
        }
      }

      // 3. Merge the durable native log with browser recovery state. Persist
      // every accepted revision so corrections and speaker updates remain
      // auditable in SQLite.
      const revisionEvents = await loadRecoveryTranscriptEvents(meetingId, folderPath);
      if (revisionEvents.length === 0) {
        throw new Error('No transcripts found for this meeting');
      }
      const transcriptsForPersistence = revisionEvents.map((event) => (
        transcriptUpdateToTranscript(event)
      ));

      // 4. Attempt audio recovery if folder path exists
      let audioRecoveryStatus: AudioRecoveryStatus | null = null;
      if (folderPath) {
        try {
          audioRecoveryStatus = await invoke<AudioRecoveryStatus>(
            'recover_audio_from_checkpoints',
            { meetingFolder: folderPath, sampleRate: 48000 }
          );
        } catch (error) {
          console.error('Audio recovery failed:', error);
          audioRecoveryStatus = {
            status: 'failed',
            chunk_count: 0,
            estimated_duration_seconds: 0,
            message: error instanceof Error ? error.message : 'Unknown error'
          };
        }
      } else {
        audioRecoveryStatus = {
          status: 'none',
          chunk_count: 0,
          estimated_duration_seconds: 0,
          message: 'No folder path available'
        };
      }

      // 5. Convert StoredTranscripts to the format expected by storageService
      const formattedTranscripts = transcriptsForPersistence.map((t, index) => ({
        ...t,
        id: t.id?.toString() || `${Date.now()}-${index}`,
        sequence_id: t.sequence_id ?? index,
        chunk_start_time: t.chunk_start_time ?? t.audio_start_time ?? 0,
        is_partial: t.is_partial ?? false,
        confidence: t.confidence ?? t.asr_confidence ?? 0,
      }));

      // 6. Save to backend database using existing save utilities
      const saveResponse = await storageService.saveMeeting(
        metadata.title,
        formattedTranscripts,
        folderPath ?? null
      );

      const savedMeetingId = saveResponse.meeting_id;

      try {
        await applyPinnedSummaryLanguageToMeeting(savedMeetingId);
      } catch (error) {
        console.warn('Failed to apply pinned summary language to recovered meeting:', error);
        toast.warning('Could not apply default summary language', {
          description: 'The recovered meeting was saved, but the default summary language was not applied.',
        });
      }

      // 7. Mark as saved in IndexedDB
      await indexedDBService.markMeetingSaved(meetingId);


      // 8. Clean up checkpoint files only after the backend has explicitly
      // verified every discovered track. The cleanup command repeats those
      // validations server-side before deleting anything.
      if (folderPath && isAudioRecoverySafeToCleanup(audioRecoveryStatus)) {
        try {
          const cleanup = await invoke<CheckpointCleanupResult>(
            'cleanup_checkpoints',
            { meetingFolder: folderPath },
          );
          console.info(
            `Cleaned verified audio checkpoints for: ${cleanup.cleaned_tracks.join(', ')}`,
          );
        } catch (error) {
          // Non-fatal and intentionally non-destructive: backend validation or
          // deletion failure leaves the checkpoint source available to retry.
          console.warn('Checkpoint cleanup was refused or failed; checkpoints retained:', error);
        }
      } else if (folderPath && audioRecoveryStatus?.status !== 'none') {
        console.warn(
          'Audio recovery was not fully verified; all checkpoints were retained.',
          audioRecoveryStatus,
        );
      }

      // 9. Remove from recoverable list
      setRecoverableMeetings(prev => prev.filter(m => m.meetingId !== meetingId));

      return {
        success: true,
        audioRecoveryStatus,
        meetingId: savedMeetingId
      };
    } catch (error) {
      console.error('Failed to recover meeting:', error);
      throw error;
    } finally {
      setIsRecovering(false);
    }
  }, []);

  /**
   * Delete a recoverable meeting
   */
  const deleteRecoverableMeeting = useCallback(async (meetingId: string): Promise<void> => {
    try {
      await indexedDBService.deleteMeeting(meetingId);
      setRecoverableMeetings(prev => prev.filter(m => m.meetingId !== meetingId));
    } catch (error) {
      console.error('Failed to delete meeting:', error);
      throw error;
    }
  }, []);

  return {
    recoverableMeetings,
    isLoading,
    isRecovering,
    checkForRecoverableTranscripts,
    recoverMeeting,
    loadMeetingTranscripts,
    deleteRecoverableMeeting
  };
}
