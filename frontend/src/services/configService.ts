/**
 * Configuration Service
 *
 * Handles all configuration-related Tauri backend calls.
 * Pure 1-to-1 wrapper - no error handling changes, exact same behavior as direct invoke calls.
 */

import { invoke } from '@tauri-apps/api/core';
import type {
  TranscriptConnectionResult,
  TranscriptModelConfig,
  TranscriptProvider,
} from '@/types/transcription-config';

export type SummaryProvider = 'ollama' | 'groq' | 'claude' | 'openrouter' | 'openai' | 'builtin-ai' | 'custom-openai';
export type SummaryKeyProvider = Exclude<SummaryProvider, 'ollama' | 'builtin-ai'>;

export interface SummaryApiKeyConfigured {
  openai: boolean;
  claude: boolean;
  groq: boolean;
  openrouter: boolean;
  customOpenai: boolean;
}

export const EMPTY_SUMMARY_API_KEY_CONFIGURED: SummaryApiKeyConfigured = {
  openai: false,
  claude: false,
  groq: false,
  openrouter: false,
  customOpenai: false,
};

export interface ModelConfig {
  provider: SummaryProvider;
  model: string;
  whisperModel: string;
  hasApiKey: boolean;
  apiKeyConfigured: SummaryApiKeyConfigured;
  ollamaEndpoint?: string | null;
  customOpenAIEndpoint?: string | null;
  customOpenAIModel?: string | null;
  customOpenAIHasApiKey?: boolean;
  maxTokens?: number | null;
  temperature?: number | null;
  topP?: number | null;
}

export interface CustomOpenAIConfig {
  endpoint: string;
  hasApiKey: boolean;
  model: string;
  maxTokens: number | null;
  temperature: number | null;
  topP: number | null;
}

export interface SummaryApiKeyMutationResult {
  provider: SummaryKeyProvider;
  hasApiKey: boolean;
  apiKeyConfigured: SummaryApiKeyConfigured;
}

export interface SummaryConnectionTestResult {
  ok: boolean;
  code: string;
  message: string;
  httpStatus?: number;
}

export interface RecordingPreferences {
  save_folder: string;
  auto_save: boolean;
  file_format: string;
  preferred_mic_device: string | null;
  preferred_system_device: string | null;
  system_audio_backend?: string | null;
}

/**
 * Configuration Service
 * Singleton service for managing app configuration
 */
export class ConfigService {
  /** Get the public transcript configuration. Saved secrets never cross IPC. */
  async getTranscriptConfig(): Promise<TranscriptModelConfig | null> {
    return invoke<TranscriptModelConfig | null>('api_get_transcript_config');
  }

  async saveTranscriptConfig(config: TranscriptModelConfig): Promise<{ status: string; message: string }> {
    return invoke<{ status: string; message: string }>('api_save_transcript_config', {
      provider: config.provider,
      model: config.model,
      streamingConfig: config.streamingConfig,
    });
  }

  async setTranscriptApiKey(
    provider: Extract<TranscriptProvider, 'deepgram' | 'openai'>,
    apiKey: string,
  ): Promise<{ hasApiKey: boolean }> {
    return invoke<{ hasApiKey: boolean }>('api_set_transcript_api_key', {
      provider,
      apiKey,
    });
  }

  async deleteTranscriptApiKey(
    provider: Extract<TranscriptProvider, 'deepgram' | 'openai'>,
  ): Promise<{ hasApiKey: boolean }> {
    return invoke<{ hasApiKey: boolean }>('api_delete_transcript_api_key', {
      provider,
    });
  }

  async testTranscriptConnection(options: {
    provider: Extract<TranscriptProvider, 'deepgram' | 'openai'>;
    apiKey?: string;
    useStoredKey: boolean;
    configOverride: TranscriptModelConfig;
  }): Promise<TranscriptConnectionResult> {
    return invoke<TranscriptConnectionResult>('api_test_transcript_connection', options);
  }

  /**
   * Get saved summary model configuration
   * @returns Promise with { provider, model, whisperModel }
   */
  async getModelConfig(): Promise<ModelConfig | null> {
    return invoke<ModelConfig | null>('api_get_model_config');
  }

  async setSummaryApiKey(provider: SummaryKeyProvider, apiKey: string): Promise<SummaryApiKeyMutationResult> {
    return invoke<SummaryApiKeyMutationResult>('api_set_summary_api_key', { provider, apiKey });
  }

  async deleteSummaryApiKey(provider: SummaryKeyProvider): Promise<SummaryApiKeyMutationResult> {
    return invoke<SummaryApiKeyMutationResult>('api_delete_summary_api_key', { provider });
  }

  /**
   * Get saved audio device preferences
   * @returns Promise with { preferred_mic_device, preferred_system_device }
   */
  async getRecordingPreferences(): Promise<RecordingPreferences> {
    return invoke<RecordingPreferences>('get_recording_preferences');
  }

  /** Persist preferences and return the backend-normalized canonical value. */
  async saveRecordingPreferences(
    preferences: RecordingPreferences,
  ): Promise<RecordingPreferences> {
    return invoke<RecordingPreferences>('set_recording_preferences', { preferences });
  }

  /**
   * Get custom OpenAI configuration
   * @returns Promise with CustomOpenAIConfig or null if not configured
   */
  async getCustomOpenAIConfig(): Promise<CustomOpenAIConfig | null> {
    return invoke<CustomOpenAIConfig | null>('api_get_custom_openai_config');
  }

  /**
   * Save custom OpenAI configuration
   * @param config - CustomOpenAIConfig to save
   * @returns Promise with result status
   */
  async saveCustomOpenAIConfig(
    config: CustomOpenAIConfig,
    replacementApiKey?: string | null,
  ): Promise<CustomOpenAIConfig> {
    return invoke<CustomOpenAIConfig>('api_save_custom_openai_config', {
      endpoint: config.endpoint,
      apiKey: replacementApiKey?.trim() || null,
      model: config.model,
      maxTokens: config.maxTokens,
      temperature: config.temperature,
      topP: config.topP,
    });
  }

  /**
   * Test custom OpenAI connection
   * @param endpoint - API endpoint URL
   * @param apiKey - Optional API key
   * @param model - Model name
   * @returns Promise with test result
   */
  async testCustomOpenAIConnection(
    endpoint: string,
    apiKey: string | null,
    model: string,
    useStoredKey: boolean,
  ): Promise<SummaryConnectionTestResult> {
    return invoke<SummaryConnectionTestResult>('api_test_custom_openai_connection', {
      endpoint,
      apiKey: apiKey?.trim() || null,
      model,
      useStoredKey,
    });
  }
}

// Export singleton instance
export const configService = new ConfigService();
