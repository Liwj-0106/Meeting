import {
  AlertTriangle,
  CheckCircle2,
  CircleHelp,
  ListTodo,
  MessageSquareText,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import type { LiveSummaryItem, LiveSummarySnapshot } from '@/services/liveSummaryService';
import {
  countAssistantEvidence,
  selectAssistantItems,
  type LiveSummaryBinding,
  type MeetingAssistantMode,
} from '@/lib/live-summary-overlay';

interface MeetingAssistantPanelProps {
  mode: Exclude<MeetingAssistantMode, 'captions'>;
  binding: LiveSummaryBinding;
  fontSize: number;
}

const KIND_PRESENTATION: Record<
  LiveSummaryItem['kind'],
  { label: string; accent: string; Icon: LucideIcon }
> = {
  topic: { label: '议题', accent: '#7DD3FC', Icon: MessageSquareText },
  decision: { label: '决定', accent: '#34D399', Icon: CheckCircle2 },
  action_item: { label: '待办', accent: '#34D399', Icon: ListTodo },
  risk: { label: '风险', accent: '#FBBF24', Icon: AlertTriangle },
  open_question: { label: '待确认', accent: 'rgba(255,255,255,0.72)', Icon: CircleHelp },
};

function formatUpdatedAt(value?: string): string {
  if (!value) return '等待更新';
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return '刚刚更新';
  return new Intl.DateTimeFormat('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  }).format(parsed);
}

function AssistantEmptyState({
  binding,
  mode,
}: Pick<MeetingAssistantPanelProps, 'binding' | 'mode'>) {
  if (binding.status === 'missing_transcript_scope') {
    return (
      <div className="max-w-[34rem] text-center">
        <p className="text-sm font-medium text-white/78">尚未建立当前录音会话</p>
        <p className="mt-1 text-xs leading-relaxed text-white/45">
          开始录音并出现首条转写后，Meetily 才能安全关联实时总结；这里不会显示上一场会议的内容。
        </p>
      </div>
    );
  }
  if (binding.status === 'waiting_for_summary') {
    return (
      <div className="max-w-[34rem] text-center">
        <p className="text-sm font-medium text-white/78">正在等待实时总结会话</p>
        <p className="mt-1 text-xs leading-relaxed text-white/45">
          请先在主窗口“设置 → 总结”中配置模型和提示词，再开始录音。
        </p>
      </div>
    );
  }
  if (binding.status === 'scope_mismatch') {
    return (
      <div className="max-w-[34rem] text-center">
        <p className="text-sm font-medium text-white/78">总结尚未关联到当前录音</p>
        <p className="mt-1 text-xs leading-relaxed text-white/45">
          检测到总结来自另一场会议，已暂时隐藏。当前录音的总结建立后会自动显示。
        </p>
      </div>
    );
  }

  const { snapshot } = binding;
  if (snapshot.lifecycle === 'unavailable' || snapshot.lifecycle === 'error') {
    return (
      <div className="max-w-[34rem] text-center" role="status">
        <p className="text-sm font-medium text-[#FBBF24]">实时总结暂不可用</p>
        <p className="mt-1 text-xs leading-relaxed text-white/45">
          请在主窗口检查总结模型与 API 密钥。字幕和录音不会受影响。
        </p>
      </div>
    );
  }
  if (snapshot.lifecycle === 'generating' || snapshot.dispatchState === 'in_flight') {
    return (
      <div className="max-w-[34rem] text-center" role="status">
        <p className="text-sm font-medium text-white/78">正在整理首批{mode === 'actions' ? '待办' : '要点'}…</p>
        <p className="mt-1 text-xs leading-relaxed text-white/45">原始字幕仍会继续更新，不必等待总结完成。</p>
      </div>
    );
  }
  return (
    <div className="max-w-[34rem] text-center">
      <p className="text-sm font-medium text-white/72">
        {mode === 'actions' ? '还没有已识别的待办' : '还没有稳定要点'}
      </p>
      <p className="mt-1 text-xs leading-relaxed text-white/42">
        {mode === 'actions'
          ? '会议中出现明确的负责人、事项或截止时间后会显示在这里。'
          : '模型形成有证据支持的议题、决定、风险或待确认问题后会显示在这里。'}
      </p>
    </div>
  );
}

function EvidenceRail({ snapshot, items }: { snapshot: LiveSummarySnapshot; items: LiveSummaryItem[] }) {
  const evidenceCount = countAssistantEvidence(items);
  const tickCount = Math.min(12, evidenceCount);
  const updatedAt = snapshot.updatedAt ?? snapshot.lastCompleteRevision?.created_at;
  return (
    <div className="flex shrink-0 items-center gap-3 px-1 pt-2 text-[10px] text-white/38" aria-label={`证据 ${evidenceCount} 条，${formatUpdatedAt(updatedAt)}`}>
      <div className="flex h-2 min-w-12 flex-1 items-end gap-[3px]" aria-hidden="true">
        <span className="h-px flex-1 bg-white/10" />
        {Array.from({ length: Math.max(1, tickCount) }, (_, index) => (
          <span
            key={index}
            className={`w-px ${index === tickCount - 1 && evidenceCount > 0 ? 'h-2 bg-[#34D399]/75' : 'h-1 bg-white/24'}`}
          />
        ))}
      </div>
      <span className="shrink-0 tabular-nums">证据 {evidenceCount}</span>
      <span aria-hidden="true">·</span>
      <time className="shrink-0 tabular-nums" dateTime={updatedAt}>更新 {formatUpdatedAt(updatedAt)}</time>
    </div>
  );
}

export function MeetingAssistantPanel({ mode, binding, fontSize }: MeetingAssistantPanelProps) {
  if (binding.status !== 'bound') {
    return (
      <div className="flex h-full min-h-0 items-center justify-center px-7 pb-4">
        <AssistantEmptyState binding={binding} mode={mode} />
      </div>
    );
  }

  const items = selectAssistantItems(binding.snapshot, mode);
  if (items.length === 0) {
    return (
      <div className="flex h-full min-h-0 flex-col px-7 pb-3">
        <div className="flex min-h-0 flex-1 items-center justify-center">
          <AssistantEmptyState binding={binding} mode={mode} />
        </div>
        <EvidenceRail snapshot={binding.snapshot} items={items} />
      </div>
    );
  }

  const visibleItems = items.slice(-4);
  return (
    <div className="flex h-full min-h-0 flex-col px-5 pb-3 pt-1 text-left">
      <div className="custom-scrollbar min-h-0 flex-1 overflow-y-auto pr-1" role="list" aria-label={mode === 'actions' ? '实时待办' : '实时会议要点'}>
        {visibleItems.map((item) => {
          const { label, accent, Icon } = KIND_PRESENTATION[item.kind];
          return (
            <article
              key={item.item_id}
              role="listitem"
              className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-2 border-b border-white/[0.07] py-2 last:border-b-0"
            >
              <Icon className="mt-0.5 h-3.5 w-3.5" style={{ color: accent }} />
              <div className="min-w-0">
                <div className="flex min-w-0 items-baseline gap-2">
                  <span className="shrink-0 text-[10px] font-semibold tracking-[0.08em]" style={{ color: accent }}>{label}</span>
                  <h2
                    className="min-w-0 truncate font-medium text-white/92"
                    style={{ fontSize: `${Math.max(13, Math.round(fontSize * 0.55))}px` }}
                  >
                    {item.title}
                  </h2>
                  {item.status === 'needs_review' ? (
                    <span className="shrink-0 text-[10px] text-[#FBBF24]">待确认</span>
                  ) : null}
                </div>
                {item.body ? <p className="mt-0.5 line-clamp-2 text-xs leading-relaxed text-white/52">{item.body}</p> : null}
                {mode === 'actions' && (item.owner || item.due_at) ? (
                  <p className="mt-1 truncate text-[10px] text-white/38">
                    {item.owner ? `负责人 ${item.owner}` : '负责人待确认'}
                    {item.due_at ? ` · 截止 ${item.due_at}` : ''}
                  </p>
                ) : null}
              </div>
            </article>
          );
        })}
      </div>
      {items.length > visibleItems.length ? (
        <p className="pt-1 text-right text-[10px] text-white/35">另有 {items.length - visibleItems.length} 项，请在主窗口查看</p>
      ) : null}
      <EvidenceRail snapshot={binding.snapshot} items={items} />
    </div>
  );
}
