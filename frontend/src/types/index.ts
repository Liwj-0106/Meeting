export interface Message {
  id: string;
  content: string;
  timestamp: string;
}

/**
 * Lifecycle of a revisable transcript event.
 *
 * Providers may add event kinds in the future, so the string fallback is
 * intentional. Known values still provide editor completion without making a
 * newer backend incompatible with an older frontend.
 */
export type TranscriptEventKind =
  | 'partial'
  | 'final'
  | 'correction'
  | 'speaker_update'
  | 'language_update'
  | 'retraction'
  | (string & {});

export interface TranscriptSpeaker {
  speaker_id: string;
  local_label?: string;
  display_name?: string;
  confidence?: number;
  status?:
    | 'unresolved'
    | 'provisional'
    | 'resolved'
    | 'user_confirmed'
    | 'renamed'
    | 'merged'
    | (string & {});
}

export interface TranscriptAsrMetadata {
  provider?: string;
  model?: string;
  confidence?: number;
  latency_ms?: number;
}

export interface TranscriptDiarizationMetadata {
  provider?: string;
  model?: string;
  model_revision?: string;
  /** Independent diarization pass revision, not the utterance revision. */
  revision?: number;
  window_id?: string;
  /** Meetily canonical 48 kHz frame coordinates. */
  window_start_frame?: number;
  window_end_frame?: number;
  status?:
    | 'pending'
    | 'running'
    | 'provisional'
    | 'resolved'
    | 'failed'
    | (string & {});
  latency_ms?: number;
}

/** Fields shared by stored transcripts and live transcript updates. */
export interface RevisableTranscriptFields {
  schema_version?: number;
  event_id?: string;
  meeting_id?: string;
  session_id?: string;
  utterance_id?: string;
  revision?: number;
  event_kind?: TranscriptEventKind;
  is_stable?: boolean;
  start_ms?: number;
  end_ms?: number;
  language?: string;
  /** Canonical source. `source` remains available for legacy events. */
  audio_source?: string;
  speaker?: TranscriptSpeaker;
  /** Convenient flat access for consumers that do not need speaker metadata. */
  speaker_id?: string;
  speaker_local_label?: string;
  speaker_display_name?: string;
  speaker_confidence?: number;
  speaker_status?: TranscriptSpeaker['status'];
  asr?: TranscriptAsrMetadata;
  /** Canonical flat fields used by the wire payload and SQLite projection. */
  asr_provider?: string;
  asr_model?: string;
  asr_confidence?: number;
  asr_latency_ms?: number;
  diarization?: TranscriptDiarizationMetadata;
  diarization_provider?: string;
  diarization_model?: string;
  diarization_model_revision?: string;
  diarization_revision?: number;
  diarization_window_id?: string;
  diarization_window_start_frame?: number;
  diarization_window_end_frame?: number;
  diarization_status?: TranscriptDiarizationMetadata['status'];
  diarization_latency_ms?: number;
  /** Older flat aliases are accepted during the backend migration period. */
  provider?: string;
  model?: string;
  latency_ms?: number;
  replaces_event_id?: string;
  provider_event_id?: string;
  created_at?: string;
  trace_id?: string;
}

export interface Transcript extends RevisableTranscriptFields {
  id: string;
  text: string;
  timestamp: string; // Wall-clock time (e.g., "14:30:05")
  /** Legacy audio source field retained for existing recordings. */
  source?: string;
  sequence_id?: number;
  chunk_start_time?: number; // Legacy field
  is_partial?: boolean;
  confidence?: number;
  // NEW: Recording-relative timestamps for playback sync
  audio_start_time?: number; // Seconds from recording start (e.g., 125.3)
  audio_end_time?: number;   // Seconds from recording start (e.g., 128.6)
  duration?: number;          // Segment duration in seconds (e.g., 3.3)
}

export interface TranscriptUpdate extends RevisableTranscriptFields {
  text: string;
  timestamp: string; // Wall-clock time for reference
  source: string;
  sequence_id: number;
  chunk_start_time: number; // Legacy field
  is_partial: boolean;
  confidence: number;
  // NEW: Recording-relative timestamps for playback sync
  audio_start_time: number; // Seconds from recording start
  audio_end_time: number;   // Seconds from recording start
  duration: number;          // Segment duration in seconds
}

export interface Block {
  id: string;
  type: string;
  content: string;
  color: string;
}

export interface Section {
  title: string;
  blocks: Block[];
}

export interface Summary {
  [key: string]: Section;
}

export interface ApiResponse {
  message: string;
  num_chunks: number;
  data: any[];
}

export interface SummaryResponse {
  status: string;
  summary: Summary;
  raw_summary?: string;
  usage?: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  };
}

// BlockNote-specific types
export type SummaryFormat = 'legacy' | 'markdown' | 'blocknote';

export interface BlockNoteBlock {
  id: string;
  type: string;
  props?: Record<string, any>;
  content?: any[];
  children?: BlockNoteBlock[];
}

export interface SummaryDataResponse {
  markdown?: string;
  summary_json?: BlockNoteBlock[];
  // Legacy format fields
  MeetingName?: string;
  _section_order?: string[];
  [key: string]: any; // For legacy section data
}

// Pagination types for optimized transcript loading
export interface MeetingMetadata {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  folder_path?: string;
}

export interface PaginatedTranscriptsResponse {
  transcripts: Transcript[];
  total_count: number;
  has_more: boolean;
}

// Transcript segment data for virtualized display
export interface TranscriptSegmentData {
  id: string;
  timestamp: number; // audio_start_time in seconds
  endTime?: number; // audio_end_time in seconds
  text: string;
  confidence?: number;
}
