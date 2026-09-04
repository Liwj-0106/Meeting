'use client';

import { RotateCcw } from 'lucide-react';
import type {
  CaptionOverlayMode,
  CaptionOverlaySettingsController,
} from '@/hooks/useCaptionOverlaySettings';

interface CaptionOverlaySettingsControlsProps {
  controller: CaptionOverlaySettingsController;
  tone?: 'light' | 'dark';
  onMousePassthroughEnabled?: () => void;
}

interface RangeControlProps {
  label: string;
  value: number;
  min: number;
  max: number;
  step?: number;
  suffix: string;
  disabled: boolean;
  tone: 'light' | 'dark';
  onChange: (value: number) => void;
}

function RangeControl({
  label,
  value,
  min,
  max,
  step = 1,
  suffix,
  disabled,
  tone,
  onChange,
}: RangeControlProps) {
  const mutedClass = tone === 'dark' ? 'text-white/55' : 'text-slate-500';

  return (
    <label className="grid gap-1.5">
      <span className="flex items-baseline justify-between gap-3 text-xs font-medium">
        <span>{label}</span>
        <span className={`tabular-nums ${mutedClass}`}>{value}{suffix}</span>
      </span>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        aria-valuetext={`${value}${suffix}`}
        disabled={disabled}
        onChange={(event) => onChange(Number(event.target.value))}
        className="h-4 w-full cursor-pointer accent-emerald-400 disabled:cursor-not-allowed disabled:opacity-40"
      />
      <span className={`flex justify-between text-[10px] tabular-nums ${mutedClass}`} aria-hidden="true">
        <span>{min}{suffix}</span>
        <span>{max}{suffix}</span>
      </span>
    </label>
  );
}

export function CaptionOverlaySettingsControls({
  controller,
  tone = 'light',
  onMousePassthroughEnabled,
}: CaptionOverlaySettingsControlsProps) {
  const {
    settings,
    isLoading,
    isSaving,
    error,
    updateSettings,
    resetSettings,
  } = controller;
  const isDark = tone === 'dark';
  const mutedClass = isDark ? 'text-white/55' : 'text-slate-500';
  const dividerClass = isDark ? 'border-white/10' : 'border-slate-200';
  const modeOptions: ReadonlyArray<{ value: CaptionOverlayMode; label: string }> = [
    { value: 'captions', label: '字幕' },
    { value: 'highlights', label: '要点' },
    { value: 'actions', label: '待办' },
  ];

  const toggleMousePassthrough = () => {
    const enabled = !settings.mouse_passthrough;
    if (enabled) onMousePassthroughEnabled?.();
    updateSettings({ mouse_passthrough: enabled }, { immediate: true });
  };

  const toggleContentProtection = () => {
    updateSettings(
      { content_protection: !settings.content_protection },
      { immediate: true },
    );
  };

  return (
    <div className={`grid gap-3 text-sm ${isDark ? 'text-white' : 'text-slate-900'}`}>
      <div className="flex items-start justify-between gap-3">
        <div>
          <p className="font-semibold leading-none">悬浮会议助手</p>
          <p className={`mt-1 text-xs ${mutedClass}`}>更改会自动保存。</p>
        </div>
        <span className={`text-[11px] ${mutedClass}`} aria-live="polite">
          {isLoading ? '正在加载…' : isSaving ? '正在保存…' : '已保存'}
        </span>
      </div>

      <fieldset className="grid gap-1.5">
        <legend className="text-xs font-medium">默认显示模式</legend>
        <div className={`grid grid-cols-3 gap-1 rounded-lg border p-1 ${dividerClass}`}>
          {modeOptions.map((option) => {
            const selected = settings.assistant_mode === option.value;
            return (
              <button
                key={option.value}
                type="button"
                aria-pressed={selected}
                disabled={isLoading || isSaving}
                onClick={() => updateSettings(
                  { assistant_mode: option.value },
                  { immediate: true },
                )}
                className={`rounded-md px-2 py-1.5 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#7DD3FC] disabled:opacity-45 ${
                  selected
                    ? isDark
                      ? 'bg-white/[0.12] text-white'
                      : 'bg-slate-900 text-white'
                    : isDark
                      ? 'text-white/50 hover:bg-white/[0.06] hover:text-white/80'
                      : 'text-slate-500 hover:bg-slate-100 hover:text-slate-800'
                }`}
              >
                {option.label}
              </button>
            );
          })}
        </div>
        <p className={`text-[11px] leading-snug ${mutedClass}`}>
          窗口内也可随时切换；要点和待办只显示与当前录音严格关联的实时总结。
        </p>
      </fieldset>

      <div className="grid grid-cols-2 gap-x-4 gap-y-3">
        <RangeControl
          label="窗口宽度"
          value={settings.window_width}
          min={360}
          max={1600}
          step={20}
          suffix=" 像素"
          disabled={isLoading}
          tone={tone}
          onChange={(window_width) => updateSettings({ window_width })}
        />
        <RangeControl
          label="窗口高度"
          value={settings.window_height}
          min={100}
          max={480}
          step={10}
          suffix=" 像素"
          disabled={isLoading}
          tone={tone}
          onChange={(window_height) => updateSettings({ window_height })}
        />
        <RangeControl
          label="内容字号"
          value={settings.font_size}
          min={16}
          max={56}
          suffix=" 像素"
          disabled={isLoading}
          tone={tone}
          onChange={(font_size) => updateSettings({ font_size })}
        />
        <RangeControl
          label="背景不透明度"
          value={settings.background_opacity}
          min={0}
          max={100}
          step={5}
          suffix="%"
          disabled={isLoading}
          tone={tone}
          onChange={(background_opacity) => updateSettings({ background_opacity })}
        />
      </div>

      <div className={`flex items-center justify-between gap-4 border-t pt-3 ${dividerClass}`}>
        <div className="min-w-0">
          <p className="text-xs font-medium">鼠标穿透</p>
          <p id="mouse-passthrough-help" className="mt-0.5 text-[11px] leading-snug text-[#FBBF24]/80">
            开启前请确认：悬浮窗将不再响应鼠标。当前主窗口设置和系统托盘始终可以关闭穿透。
          </p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={settings.mouse_passthrough}
          aria-label={settings.mouse_passthrough ? '关闭鼠标穿透' : '开启鼠标穿透'}
          aria-describedby="mouse-passthrough-help"
          disabled={isLoading || isSaving}
          onClick={toggleMousePassthrough}
          className={`relative h-6 w-11 shrink-0 rounded-full border transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-emerald-400 disabled:cursor-not-allowed disabled:opacity-50 ${
            settings.mouse_passthrough
              ? 'border-emerald-300/70 bg-emerald-400'
              : isDark
                ? 'border-white/20 bg-white/10'
                : 'border-slate-300 bg-slate-200'
          }`}
        >
          <span
            className={`absolute top-0.5 h-[18px] w-[18px] rounded-full bg-white shadow-sm transition-transform ${
              settings.mouse_passthrough ? 'translate-x-5' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      <div className={`flex items-center justify-between gap-4 border-t pt-3 ${dividerClass}`}>
        <div className="min-w-0">
          <p className="text-xs font-medium">屏幕分享保护（实验性）</p>
          <p className={`mt-0.5 text-[11px] leading-snug ${mutedClass}`}>
            请求系统阻止其他应用捕获悬浮窗；实际效果取决于录屏或会议软件。
          </p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={settings.content_protection}
          aria-label="屏幕分享保护"
          disabled={isLoading || isSaving}
          onClick={toggleContentProtection}
          className={`relative h-6 w-11 shrink-0 rounded-full border transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-emerald-400 disabled:cursor-not-allowed disabled:opacity-50 ${
            settings.content_protection
              ? 'border-emerald-300/70 bg-emerald-400'
              : isDark
                ? 'border-white/20 bg-white/10'
                : 'border-slate-300 bg-slate-200'
          }`}
        >
          <span
            className={`absolute top-0.5 h-[18px] w-[18px] rounded-full bg-white shadow-sm transition-transform ${
              settings.content_protection ? 'translate-x-5' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      <div className="flex min-h-6 items-center justify-between gap-3">
        <p className={`text-[11px] ${error ? 'text-red-400' : mutedClass}`} role="status">
          {error ? '会议助手设置保存失败。' : '背景为 0% 时仍保留文字阴影。'}
        </p>
        <button
          type="button"
          onClick={resetSettings}
          disabled={isLoading || isSaving}
          className={`inline-flex items-center gap-1 rounded-md px-2 py-1 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-emerald-400 disabled:opacity-40 ${
            isDark ? 'hover:bg-white/10' : 'hover:bg-slate-100'
          }`}
        >
          <RotateCcw className="h-3 w-3" />
          恢复默认
        </button>
      </div>
    </div>
  );
}
