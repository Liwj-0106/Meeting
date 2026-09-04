'use client';

import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Captions, Loader2, Settings2 } from 'lucide-react';
import { toast } from 'sonner';
import { CaptionOverlaySettingsControls } from '@/components/CaptionOverlaySettingsControls';
import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { useCaptionOverlaySettings } from '@/hooks/useCaptionOverlaySettings';

interface CaptionOverlayVisibility {
  visible: boolean;
}

export function FloatingCaptionToggle() {
  const captionSettings = useCaptionOverlaySettings();
  const [visible, setVisible] = useState(false);
  const [isUpdating, setIsUpdating] = useState(false);
  const [initialized, setInitialized] = useState(false);
  const visibilityRevisionRef = useRef(0);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;

    const initialize = async () => {
      try {
        const stopListening = await listen<CaptionOverlayVisibility>(
          'caption-overlay-visibility-changed',
          (event) => {
            visibilityRevisionRef.current += 1;
            if (!disposed) {
              setVisible(event.payload.visible);
            }
          },
        );

        if (disposed) {
          stopListening();
          return;
        }

        unlisten = stopListening;

        const revisionBeforeQuery = visibilityRevisionRef.current;
        const currentVisibility = await invoke<boolean>('is_caption_overlay_visible');
        if (!disposed && revisionBeforeQuery === visibilityRevisionRef.current) {
          setVisible(currentVisibility);
        }
      } catch (error) {
        console.error('Failed to initialize live caption controls:', error);
      } finally {
        if (!disposed) setInitialized(true);
      }
    };

    initialize();

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const handleToggle = async () => {
    if (isUpdating) return;

    setIsUpdating(true);
    const revisionBeforeCommand = visibilityRevisionRef.current;
    try {
      const nextVisibility = await invoke<boolean>('set_caption_overlay_visible', {
        visible: !visible,
      });
      if (revisionBeforeCommand === visibilityRevisionRef.current) {
        setVisible(nextVisibility);
      }
    } catch (error) {
      console.error('Failed to toggle live captions:', error);
      toast.error('无法切换悬浮会议助手', {
        description: '请稍后重试，或从系统托盘显示或隐藏实时字幕。',
      });
    } finally {
      setIsUpdating(false);
    }
  };

  return (
    <>
      <Button
        variant={visible ? 'default' : 'outline'}
        size="sm"
        onClick={handleToggle}
        disabled={isUpdating || !initialized}
        title={visible ? '隐藏悬浮会议助手' : '显示悬浮会议助手'}
        aria-pressed={visible}
      >
        {isUpdating ? <Loader2 className="animate-spin" /> : <Captions />}
        <span className="hidden md:inline">悬浮助手</span>
      </Button>

      <Popover>
        <PopoverTrigger asChild>
          <Button
            variant="outline"
            size="sm"
            className="px-2"
            disabled={captionSettings.isLoading}
            title={captionSettings.settings.mouse_passthrough
              ? '鼠标穿透已开启，打开设置可关闭'
              : '悬浮会议助手设置'}
            aria-label={captionSettings.settings.mouse_passthrough
              ? '鼠标穿透已开启，打开设置可关闭'
              : '悬浮会议助手设置'}
          >
            <Settings2 className={captionSettings.settings.mouse_passthrough
              ? 'text-emerald-600'
              : undefined}
            />
          </Button>
        </PopoverTrigger>
        <PopoverContent align="start" sideOffset={8} className="w-[390px] p-4">
          <CaptionOverlaySettingsControls controller={captionSettings} />
        </PopoverContent>
      </Popover>
    </>
  );
}
