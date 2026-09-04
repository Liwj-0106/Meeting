'use client';

import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export type CaptionOverlayMode = 'captions' | 'highlights' | 'actions';

export interface CaptionOverlaySettings {
  window_width: number;
  window_height: number;
  font_size: number;
  background_opacity: number;
  mouse_passthrough: boolean;
  content_protection: boolean;
  assistant_mode: CaptionOverlayMode;
  window_x: number | null;
  window_y: number | null;
}

export const DEFAULT_CAPTION_OVERLAY_SETTINGS: CaptionOverlaySettings = {
  window_width: 900,
  window_height: 180,
  font_size: 30,
  background_opacity: 80,
  mouse_passthrough: false,
  content_protection: false,
  assistant_mode: 'captions',
  window_x: null,
  window_y: null,
};

interface UpdateOptions {
  immediate?: boolean;
}

export interface CaptionOverlaySettingsController {
  settings: CaptionOverlaySettings;
  isLoading: boolean;
  isSaving: boolean;
  error: string | null;
  updateSettings: (
    patch: Partial<CaptionOverlaySettings>,
    options?: UpdateOptions,
  ) => void;
  resetSettings: () => void;
}

export function useCaptionOverlaySettings(): CaptionOverlaySettingsController {
  const [settings, setSettings] = useState(DEFAULT_CAPTION_OVERLAY_SETTINGS);
  const [isLoading, setIsLoading] = useState(true);
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const settingsRef = useRef(settings);
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const revisionRef = useRef(0);
  const mountedRef = useRef(true);

  const applySettings = useCallback((nextSettings: CaptionOverlaySettings) => {
    settingsRef.current = nextSettings;
    if (mountedRef.current) setSettings(nextSettings);
  }, []);

  const persistSettings = useCallback(async (
    nextSettings: CaptionOverlaySettings,
    revision: number,
  ) => {
    if (mountedRef.current) setIsSaving(true);

    try {
      const savedSettings = await invoke<CaptionOverlaySettings>(
        'set_caption_overlay_settings',
        { settings: nextSettings },
      );

      if (revision === revisionRef.current) {
        applySettings(savedSettings);
        if (mountedRef.current) setError(null);
      }
    } catch (saveError) {
      console.error('Failed to save caption overlay settings:', saveError);
      if (revision === revisionRef.current && mountedRef.current) {
        setError(saveError instanceof Error ? saveError.message : String(saveError));
        try {
          const currentSettings = await invoke<CaptionOverlaySettings>(
            'get_caption_overlay_settings',
          );
          if (revision === revisionRef.current) applySettings(currentSettings);
        } catch (reloadError) {
          console.error('Failed to reload caption overlay settings:', reloadError);
        }
      }
    } finally {
      if (revision === revisionRef.current && mountedRef.current) {
        setIsSaving(false);
      }
    }
  }, [applySettings]);

  const updateSettings = useCallback((
    patch: Partial<CaptionOverlaySettings>,
    options?: UpdateOptions,
  ) => {
    const nextSettings = { ...settingsRef.current, ...patch };
    const revision = ++revisionRef.current;
    applySettings(nextSettings);
    if (mountedRef.current) setError(null);

    if (saveTimerRef.current) clearTimeout(saveTimerRef.current);

    if (options?.immediate) {
      saveTimerRef.current = null;
      void persistSettings(nextSettings, revision);
      return;
    }

    saveTimerRef.current = setTimeout(() => {
      saveTimerRef.current = null;
      void persistSettings(nextSettings, revision);
    }, 180);
  }, [applySettings, persistSettings]);

  const resetSettings = useCallback(() => {
    updateSettings(DEFAULT_CAPTION_OVERLAY_SETTINGS, { immediate: true });
  }, [updateSettings]);

  useEffect(() => {
    mountedRef.current = true;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    let eventRevision = 0;

    const initialize = async () => {
      try {
        const stopListening = await listen<CaptionOverlaySettings>(
          'caption-overlay-settings-changed',
          (event) => {
            eventRevision += 1;
            if (!disposed) applySettings(event.payload);
          },
        );

        if (disposed) {
          stopListening();
          return;
        }

        unlisten = stopListening;
        const revisionBeforeQuery = eventRevision;
        const loadedSettings = await invoke<CaptionOverlaySettings>(
          'get_caption_overlay_settings',
        );
        if (!disposed && revisionBeforeQuery === eventRevision) {
          applySettings(loadedSettings);
        }
      } catch (loadError) {
        console.error('Failed to load caption overlay settings:', loadError);
        if (!disposed) {
          setError(loadError instanceof Error ? loadError.message : String(loadError));
        }
      } finally {
        if (!disposed) setIsLoading(false);
      }
    };

    void initialize();

    return () => {
      disposed = true;
      mountedRef.current = false;
      unlisten?.();
      if (saveTimerRef.current) clearTimeout(saveTimerRef.current);
    };
  }, [applySettings]);

  return {
    settings,
    isLoading,
    isSaving,
    error,
    updateSettings,
    resetSettings,
  };
}
