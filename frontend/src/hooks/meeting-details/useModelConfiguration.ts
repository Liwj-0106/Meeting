import { useState, useEffect, useCallback } from 'react';
import { ModelConfig } from '@/components/ModelSettingsModal';
import { invoke as invokeTauri } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import Analytics from '@/lib/analytics';
import {
  EMPTY_SUMMARY_API_KEY_CONFIGURED,
  type CustomOpenAIConfig,
} from '@/services/configService';

interface UseModelConfigurationProps {
  serverAddress: string | null;
}

export function useModelConfiguration({ serverAddress }: UseModelConfigurationProps) {
  // Note: No hardcoded defaults - DB is the source of truth
  const [modelConfig, setModelConfig] = useState<ModelConfig>({
    provider: 'ollama',
    model: '', // Empty until loaded from DB
    whisperModel: 'large-v3',
    hasApiKey: false,
    apiKeyConfigured: EMPTY_SUMMARY_API_KEY_CONFIGURED,
  });
  const [isLoading, setIsLoading] = useState(true);
  const [, setError] = useState<string>('');

  // Fetch model configuration on mount and when serverAddress changes
  useEffect(() => {
    const fetchModelConfig = async () => {
      setIsLoading(true);
      try {
        const data = await invokeTauri('api_get_model_config', {}) as any;
        if (data && data.provider !== null) {
          // Fetch custom OpenAI config if provider is custom-openai
          if (data.provider === 'custom-openai') {
            try {
              const customConfig = await invokeTauri('api_get_custom_openai_config') as CustomOpenAIConfig | null;
              if (customConfig) {
                data.customOpenAIEndpoint = customConfig.endpoint || null;
                data.customOpenAIModel = customConfig.model || null;
                data.customOpenAIHasApiKey = customConfig.hasApiKey;
                data.hasApiKey = customConfig.hasApiKey;
                data.apiKeyConfigured = {
                  ...data.apiKeyConfigured,
                  customOpenai: customConfig.hasApiKey,
                };
                data.maxTokens = customConfig.maxTokens || null;
                data.temperature = customConfig.temperature || null;
                data.topP = customConfig.topP || null;
                // For custom-openai, model field should match customOpenAIModel
                data.model = customConfig.model || data.model;
              }
            } catch (err) {
              console.error('Failed to fetch custom OpenAI config:', err);
            }
          }

          setModelConfig(data);
        } else {
          console.warn('⚠️ No model config found in database, using defaults');
        }
      } catch (error) {
        console.error('❌ Failed to fetch model config:', error);
      } finally {
        setIsLoading(false);
      }
    };

    fetchModelConfig();
  }, [serverAddress]);

  // Listen for model config updates from other components
  useEffect(() => {
    const setupListener = async () => {
      const { listen } = await import('@tauri-apps/api/event');
      const unlisten = await listen<ModelConfig>('model-config-updated', (event) => {
        setModelConfig(event.payload);
      });

      return unlisten;
    };

    let cleanup: (() => void) | undefined;
    setupListener().then(fn => cleanup = fn);

    return () => {
      cleanup?.();
    };
  }, []);

  // Save model configuration
  const handleSaveModelConfig = useCallback(async (updatedConfig?: ModelConfig) => {
    try {
      const configToSave = updatedConfig || modelConfig;
      const payload = {
        provider: configToSave.provider,
        model: configToSave.model,
        whisperModel: configToSave.whisperModel,
        ollamaEndpoint: configToSave.ollamaEndpoint ?? null
      };

      // Track model configuration change
      if (updatedConfig && (
        updatedConfig.provider !== modelConfig.provider ||
        updatedConfig.model !== modelConfig.model
      )) {
        await Analytics.trackModelChanged(
          modelConfig.provider,
          modelConfig.model,
          updatedConfig.provider,
          updatedConfig.model
        );
      }

      await invokeTauri('api_save_model_config', {
        provider: payload.provider,
        model: payload.model,
        whisperModel: payload.whisperModel,
        ollamaEndpoint: payload.ollamaEndpoint,
      });

      setModelConfig(configToSave);

      // Emit event to sync other components
      const { emit } = await import('@tauri-apps/api/event');
      await emit('model-config-updated', configToSave);

      toast.success("Summary settings Saved successfully");

      await Analytics.trackSettingsChanged('model_config', `${payload.provider}_${payload.model}`);
    } catch (error) {
      console.error('Failed to save model config:', error);
      toast.error("Failed to save summary settings");
      if (error instanceof Error) {
        setError(error.message);
      } else {
        setError('Failed to save model config: Unknown error');
      }
    }
  }, [modelConfig]);

  return {
    modelConfig,
    setModelConfig,
    handleSaveModelConfig,
    isLoading,
  };
}
