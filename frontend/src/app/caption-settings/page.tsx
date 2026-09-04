'use client';

import { useCallback, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { GripHorizontal, X } from 'lucide-react';
import { CaptionOverlaySettingsControls } from '@/components/CaptionOverlaySettingsControls';
import { useCaptionOverlaySettings } from '@/hooks/useCaptionOverlaySettings';

export default function CaptionSettingsPage() {
  const captionSettings = useCaptionOverlaySettings();
  const hideSettings = useCallback(() => {
    invoke('set_caption_settings_visible', { visible: false }).catch(console.error);
  }, []);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') hideSettings();
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [hideSettings]);

  return (
    <main className="h-screen w-screen select-none p-2 text-white">
      <section
        className="flex h-full w-full flex-col overflow-hidden rounded-2xl border border-white/15 bg-[#0b0e13]/[0.97] shadow-2xl backdrop-blur-xl"
        role="dialog"
        aria-label="悬浮会议助手设置"
      >
        <header
          className="grid h-8 shrink-0 grid-cols-[1fr_auto_1fr] items-center border-b border-white/10 px-3 text-white/55"
          data-tauri-drag-region
        >
          <span className="text-[11px] font-medium uppercase tracking-[0.14em]" data-tauri-drag-region>
            会议助手设置
          </span>
          <GripHorizontal className="h-4 w-4 opacity-45" data-tauri-drag-region />
          <button
            type="button"
            onClick={hideSettings}
            className="justify-self-end rounded-md p-1 text-white/60 transition-colors hover:bg-white/10 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-emerald-400"
            aria-label="关闭会议助手设置"
            title="关闭"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </header>

        <div className="custom-scrollbar min-h-0 flex-1 overflow-y-auto p-4">
          <CaptionOverlaySettingsControls controller={captionSettings} tone="dark" />
        </div>
      </section>
    </main>
  );
}
