import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  TranslationEvent,
  TranslationSourceSnapshot,
} from '@/lib/translation-events';

export type LiveTranslationProvider = 'openai' | 'openai_compatible';

/** Public configuration only. The saved credential is never part of this DTO. */
export interface LiveTranslationSettingsView {
  enabled: boolean;
  source_language: string;
  target_language: 'zh-CN' | string;
  provider: LiveTranslationProvider;
  model: string;
  endpoint: string;
  has_api_key: boolean;
}

export type LiveTranslationSettingsInput = Omit<LiveTranslationSettingsView, 'has_api_key'>;

export interface LiveTranslationAccepted {
  request_id: string;
  source: TranslationSourceSnapshot & { retracted: boolean };
  target_language: string;
  generation: number;
  state: 'queued' | 'duplicate' | 'reused' | 'retracted';
}

export interface LiveTranslationFrontendError {
  code: string;
  message: string;
  retryable: boolean;
}

const fallbackError = (error: unknown): LiveTranslationFrontendError => {
  if (
    typeof error === 'object'
    && error !== null
    && typeof (error as { code?: unknown }).code === 'string'
    && typeof (error as { message?: unknown }).message === 'string'
    && typeof (error as { retryable?: unknown }).retryable === 'boolean'
  ) {
    return error as LiveTranslationFrontendError;
  }
  return {
    code: 'translation_unknown_error',
    message: '实时翻译暂时不可用，请检查设置后重试。',
    retryable: true,
  };
};

class LiveTranslationService {
  getSettings(): Promise<LiveTranslationSettingsView> {
    return invoke<LiveTranslationSettingsView>('api_get_live_translation_settings');
  }

  saveSettings(settings: LiveTranslationSettingsInput): Promise<LiveTranslationSettingsView> {
    return invoke<LiveTranslationSettingsView>('api_save_live_translation_settings', { settings });
  }

  setApiKey(apiKey: string): Promise<LiveTranslationSettingsView> {
    return invoke<LiveTranslationSettingsView>('api_set_live_translation_api_key', { apiKey });
  }

  clearApiKey(): Promise<LiveTranslationSettingsView> {
    return invoke<LiveTranslationSettingsView>('api_clear_live_translation_api_key');
  }

  queue(eventId: string): Promise<LiveTranslationAccepted> {
    return invoke<LiveTranslationAccepted>('api_queue_live_caption_translation', { eventId });
  }

  onSettingsChanged(
    callback: (settings: LiveTranslationSettingsView) => void,
  ): Promise<UnlistenFn> {
    return listen<LiveTranslationSettingsView>(
      'live-translation-settings-changed',
      (event) => callback(event.payload),
    );
  }

  onTranslation(callback: (event: TranslationEvent) => void): Promise<UnlistenFn> {
    return listen<TranslationEvent>('live-translation-update', (event) => callback(event.payload));
  }

  toFrontendError(error: unknown): LiveTranslationFrontendError {
    return fallbackError(error);
  }
}

export const liveTranslationService = new LiveTranslationService();
