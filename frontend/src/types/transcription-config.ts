export type TranscriptProvider =
  | 'localWhisper'
  | 'parakeet'
  | 'deepgram'
  | 'openai';

export type StreamingLatencyMode = 'minimal' | 'low' | 'balanced' | 'high';

export interface StreamingProviderConfig {
  endpointOverride: string | null;
  model: string;
  language: string;
  diarization: boolean;
  latencyMode: StreamingLatencyMode;
  keywords: string[];
}

export interface TranscriptFallbackConfig {
  enabled: boolean;
  provider: 'parakeet' | 'localWhisper';
  model: string;
}

export interface TranscriptStreamingConfig {
  schemaVersion: 1;
  providers: {
    deepgram: StreamingProviderConfig;
    openai: StreamingProviderConfig;
  };
  fallback: TranscriptFallbackConfig;
}

export interface TranscriptKeyStatus {
  deepgram: boolean;
  openai: boolean;
}

/**
 * Public configuration returned to the WebView.
 *
 * Deliberately contains only key-presence flags. The saved secret is owned by
 * Rust and must never be added to this type.
 */
export interface TranscriptModelConfig {
  provider: TranscriptProvider;
  model: string;
  streamingConfig: TranscriptStreamingConfig;
  hasApiKey: boolean;
  apiKeyConfigured?: TranscriptKeyStatus;
}

export interface TranscriptConnectionResult {
  ok: boolean;
  code: string;
  message: string;
  latencyMs?: number;
}

export const LOCAL_TRANSCRIPT_DEFAULTS = {
  parakeet: 'parakeet-tdt-0.6b-v3-int8',
  localWhisper: 'small-q5_1',
} as const;

export function createDefaultStreamingConfig(): TranscriptStreamingConfig {
  return {
    schemaVersion: 1,
    providers: {
      deepgram: {
        endpointOverride: null,
        model: 'nova-3',
        language: 'auto',
        diarization: true,
        latencyMode: 'balanced',
        keywords: [],
      },
      openai: {
        endpointOverride: null,
        model: 'gpt-live-transcribe',
        language: 'auto',
        diarization: false,
        latencyMode: 'low',
        keywords: [],
      },
    },
    fallback: {
      enabled: true,
      provider: 'parakeet',
      model: LOCAL_TRANSCRIPT_DEFAULTS.parakeet,
    },
  };
}

export function createDefaultTranscriptConfig(): TranscriptModelConfig {
  return {
    provider: 'parakeet',
    model: LOCAL_TRANSCRIPT_DEFAULTS.parakeet,
    streamingConfig: createDefaultStreamingConfig(),
    hasApiKey: false,
    apiKeyConfigured: {
      deepgram: false,
      openai: false,
    },
  };
}

export function isStreamingTranscriptProvider(
  provider: TranscriptProvider,
): provider is 'deepgram' | 'openai' {
  return provider === 'deepgram' || provider === 'openai';
}

export function configuredKeyForProvider(
  config: TranscriptModelConfig,
  provider: 'deepgram' | 'openai',
): boolean {
  return config.apiKeyConfigured?.[provider]
    ?? (config.provider === provider && config.hasApiKey);
}

export function normalizeTranscriptConfig(
  value: Partial<TranscriptModelConfig> | null | undefined,
): TranscriptModelConfig {
  const defaults = createDefaultTranscriptConfig();
  if (!value) return defaults;

  const provider = value.provider ?? defaults.provider;
  const streaming = value.streamingConfig;
  const deepgram = streaming?.providers?.deepgram;
  const openai = streaming?.providers?.openai;

  return {
    provider,
    model: value.model || (
      provider === 'deepgram'
        ? defaults.streamingConfig.providers.deepgram.model
        : provider === 'openai'
          ? defaults.streamingConfig.providers.openai.model
          : provider === 'localWhisper'
            ? LOCAL_TRANSCRIPT_DEFAULTS.localWhisper
            : LOCAL_TRANSCRIPT_DEFAULTS.parakeet
    ),
    hasApiKey: value.hasApiKey ?? false,
    apiKeyConfigured: {
      deepgram: value.apiKeyConfigured?.deepgram
        ?? (provider === 'deepgram' && (value.hasApiKey ?? false)),
      openai: value.apiKeyConfigured?.openai
        ?? (provider === 'openai' && (value.hasApiKey ?? false)),
    },
    streamingConfig: {
      schemaVersion: 1,
      providers: {
        deepgram: {
          ...defaults.streamingConfig.providers.deepgram,
          ...deepgram,
          keywords: [...(deepgram?.keywords ?? [])],
        },
        openai: {
          ...defaults.streamingConfig.providers.openai,
          ...openai,
          keywords: [...(openai?.keywords ?? [])],
          diarization: false,
        },
      },
      fallback: {
        ...defaults.streamingConfig.fallback,
        ...streaming?.fallback,
      },
    },
  };
}
