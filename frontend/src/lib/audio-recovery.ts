export interface AudioTrackRecoveryStatus {
  track: string;
  status: string;
  chunk_count: number;
  audio_file_path?: string;
  verified: boolean;
  message: string;
}

export interface AudioRecoveryStatus {
  status: string;
  chunk_count: number;
  estimated_duration_seconds: number;
  audio_file_path?: string;
  message: string;
  tracks?: AudioTrackRecoveryStatus[];
  cleanup_ready?: boolean;
}

export interface CheckpointCleanupResult {
  cleaned_tracks: string[];
  checkpoint_root_removed: boolean;
  message: string;
}

/**
 * Destructive cleanup is deliberately fail-closed. In particular, a legacy
 * backend response that lacks per-track verification can never authorize it.
 */
export function isAudioRecoverySafeToCleanup(
  recovery: AudioRecoveryStatus | null,
): recovery is AudioRecoveryStatus & { tracks: AudioTrackRecoveryStatus[]; cleanup_ready: true } {
  return recovery?.status === 'success'
    && recovery.cleanup_ready === true
    && Array.isArray(recovery.tracks)
    && recovery.tracks.length > 0
    && recovery.tracks.every((track) => (
      track.status === 'success'
      && track.verified === true
      && track.chunk_count > 0
      && typeof track.audio_file_path === 'string'
      && track.audio_file_path.length > 0
    ));
}
