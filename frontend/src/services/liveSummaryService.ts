import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export const LIVE_SUMMARY_STATE_EVENT = 'live-summary-state';

export type LiveSummaryItemKind =
  | 'topic'
  | 'decision'
  | 'action_item'
  | 'risk'
  | 'open_question';

export type LiveSummaryItemStatus = 'active' | 'needs_review' | 'retracted';

export interface LiveSummaryScope {
  kind: 'meeting' | 'recording_session';
  id: string;
}

export interface LiveSummaryEvidence {
  scope: LiveSummaryScope;
  utterance_id: string;
  source_revision: number;
  source_event_id: string;
  source_text_hash: string;
}

export interface LiveSummaryItem {
  item_id: string;
  kind: LiveSummaryItemKind;
  title: string;
  body: string;
  owner?: string;
  due_at?: string;
  status: LiveSummaryItemStatus;
  evidence: LiveSummaryEvidence[];
}

export interface LiveSummaryRevision {
  summary_revision_id: string;
  scope: LiveSummaryScope;
  revision: number;
  generation: number;
  transcript_cursor: number;
  snapshot_hash: string;
  revision_type: 'live' | 'final';
  provider: string;
  model?: string;
  created_at: string;
  items: LiveSummaryItem[];
}

export interface LiveSummaryError {
  code: string;
  message: string;
  retryable: boolean;
}

export interface LiveSummaryProviderAvailability {
  available: boolean;
  provider: string;
  model?: string;
  error?: LiveSummaryError;
}

export interface LiveSummarySnapshot {
  schemaVersion: number;
  scope: LiveSummaryScope;
  meetingId?: string;
  sessionScopeId?: string;
  templateId?: string;
  lifecycle: 'active' | 'generating' | 'unavailable' | 'finalized' | 'error';
  dispatchState: 'idle' | 'in_flight' | 'deferred';
  provider: LiveSummaryProviderAvailability;
  transcriptCursor: number;
  summaryRevision: number;
  generation: number;
  finalized: boolean;
  recovered: boolean;
  updatedAt?: string;
  lastCompleteRevision?: LiveSummaryRevision;
  items: LiveSummaryItem[];
  error?: LiveSummaryError;
}

export interface LiveSummaryStateEvent {
  schemaVersion: number;
  kind:
    | 'session_started'
    | 'transcript_accepted'
    | 'summary_committed'
    | 'summary_failed'
    | 'final_reconcile_requested';
  snapshot: LiveSummarySnapshot;
}

export interface LiveSummaryPreferences {
  templateId: string;
  hasCustomPrompt: boolean;
}

export interface RecordingSummaryBindingTicket {
  scopeId: string;
  handle: string;
}

export interface RecordingLiveSummarySnapshot {
  state: 'active' | 'pending_binding';
  bindable: boolean;
  summary: LiveSummarySnapshot;
}

// This renderer capability is intentionally process-memory only. A WebView or
// application restart yields an honest empty state instead of reviving a stale
// binding handle from browser storage.
let activeRecordingBindingTicket: RecordingSummaryBindingTicket | null = null;

class LiveSummaryService {
  startSession(meetingId: string, templateId?: string): Promise<LiveSummarySnapshot> {
    return invoke<LiveSummarySnapshot>('api_live_summary_start_session', {
      meetingId,
      templateId: templateId ?? null,
    });
  }

  requestFinalReconcile(meetingId: string): Promise<LiveSummarySnapshot> {
    return invoke<LiveSummarySnapshot>('api_live_summary_request_final_reconcile', { meetingId });
  }

  getCurrent(meetingId: string): Promise<LiveSummarySnapshot> {
    return invoke<LiveSummarySnapshot>('api_live_summary_get_current', { meetingId });
  }

  getPreferences(): Promise<LiveSummaryPreferences> {
    return invoke<LiveSummaryPreferences>('api_live_summary_get_preferences');
  }

  savePreferences(
    templateId: string,
    replacementPrompt?: string,
  ): Promise<LiveSummaryPreferences> {
    return invoke<LiveSummaryPreferences>('api_live_summary_save_preferences', {
      templateId,
      replacementPrompt: replacementPrompt ?? null,
    });
  }

  deleteCustomPrompt(): Promise<LiveSummaryPreferences> {
    return invoke<LiveSummaryPreferences>('api_live_summary_delete_custom_prompt');
  }

  prepareRecording(): Promise<RecordingLiveSummarySnapshot[]> {
    return invoke<RecordingLiveSummarySnapshot[]>('api_live_summary_prepare_recording');
  }

  listRecordingScopes(): Promise<RecordingLiveSummarySnapshot[]> {
    return invoke<RecordingLiveSummarySnapshot[]>('api_live_summary_list_recording_scopes');
  }

  issueRecordingBindingTicket(meetingFolder: string): Promise<RecordingSummaryBindingTicket> {
    return invoke<RecordingSummaryBindingTicket>('api_live_summary_issue_binding_ticket', {
      meetingFolder,
    });
  }

  rememberRecordingBindingTicket(ticket: RecordingSummaryBindingTicket | null): void {
    activeRecordingBindingTicket = ticket ? { ...ticket } : null;
  }

  getRecordingBindingTicket(): RecordingSummaryBindingTicket | null {
    return activeRecordingBindingTicket ? { ...activeRecordingBindingTicket } : null;
  }

  clearRecordingBindingTicket(expectedHandle?: string): void {
    if (
      expectedHandle !== undefined
      && activeRecordingBindingTicket?.handle !== expectedHandle
    ) {
      return;
    }
    activeRecordingBindingTicket = null;
  }

  onState(callback: (event: LiveSummaryStateEvent) => void): Promise<UnlistenFn> {
    return listen<LiveSummaryStateEvent>(LIVE_SUMMARY_STATE_EVENT, ({ payload }) => {
      callback(payload);
    });
  }
}

export const liveSummaryService = new LiveSummaryService();
