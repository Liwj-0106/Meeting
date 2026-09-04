'use client';

import { Captions, ListChecks, ListTodo } from 'lucide-react';
import type { KeyboardEvent } from 'react';
import type { MeetingAssistantMode } from '@/lib/live-summary-overlay';

interface MeetingAssistantModeSwitchProps {
  mode: MeetingAssistantMode;
  onModeChange: (mode: MeetingAssistantMode) => void;
  disabled?: boolean;
}

const MODE_OPTIONS: ReadonlyArray<{
  mode: MeetingAssistantMode;
  label: string;
  shortcut: string;
  Icon: typeof Captions;
}> = [
  { mode: 'captions', label: '字幕', shortcut: 'Alt+1', Icon: Captions },
  { mode: 'highlights', label: '要点', shortcut: 'Alt+2', Icon: ListChecks },
  { mode: 'actions', label: '待办', shortcut: 'Alt+3', Icon: ListTodo },
];

export function MeetingAssistantModeSwitch({
  mode,
  onModeChange,
  disabled = false,
}: MeetingAssistantModeSwitchProps) {
  const moveSelection = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    let nextIndex = index;
    if (event.key === 'ArrowLeft') nextIndex = (index + MODE_OPTIONS.length - 1) % MODE_OPTIONS.length;
    else if (event.key === 'ArrowRight') nextIndex = (index + 1) % MODE_OPTIONS.length;
    else if (event.key === 'Home') nextIndex = 0;
    else if (event.key === 'End') nextIndex = MODE_OPTIONS.length - 1;
    else return;

    event.preventDefault();
    onModeChange(MODE_OPTIONS[nextIndex].mode);
    const tab = event.currentTarget.parentElement?.querySelectorAll<HTMLButtonElement>('[role="tab"]')[nextIndex];
    tab?.focus();
  };

  return (
    <div
      role="tablist"
      aria-label="会议助手显示模式"
      className="inline-flex h-7 items-center rounded-lg border border-white/10 bg-black/25 p-0.5 shadow-inner shadow-black/30"
    >
      {MODE_OPTIONS.map(({ mode: option, label, shortcut, Icon }, index) => {
        const selected = mode === option;
        return (
          <button
            key={option}
            type="button"
            role="tab"
            aria-selected={selected}
            aria-label={`${label}模式，快捷键 ${shortcut}`}
            tabIndex={selected ? 0 : -1}
            disabled={disabled}
            onClick={() => onModeChange(option)}
            onKeyDown={(event) => moveSelection(event, index)}
            className={`inline-flex h-6 items-center gap-1 rounded-md px-2 text-[11px] font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#7DD3FC] disabled:opacity-45 ${
              selected
                ? 'bg-white/[0.12] text-white shadow-sm shadow-black/25'
                : 'text-white/48 hover:bg-white/[0.06] hover:text-white/80'
            }`}
          >
            <Icon className="h-3 w-3" aria-hidden="true" />
            <span>{label}</span>
          </button>
        );
      })}
    </div>
  );
}

