'use client';

import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  AlertTriangle,
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  CircleHelp,
  ClipboardList,
  Loader2,
  Play,
  RefreshCw,
  ShieldCheck,
  Sparkles,
  Target,
} from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import {
  liveSummaryService,
  type LiveSummaryItem,
  type LiveSummaryItemKind,
  type LiveSummarySnapshot,
} from '@/services/liveSummaryService';

interface LiveSummaryPanelProps {
  meetingId: string;
}

const KIND_META: Record<LiveSummaryItemKind, { label: string; icon: typeof Target; accent: string }> = {
  topic: { label: '议题', icon: ClipboardList, accent: 'text-sky-700 bg-sky-50 border-sky-100' },
  decision: { label: '决策', icon: CheckCircle2, accent: 'text-emerald-700 bg-emerald-50 border-emerald-100' },
  action_item: { label: '行动项', icon: Target, accent: 'text-violet-700 bg-violet-50 border-violet-100' },
  risk: { label: '风险', icon: AlertTriangle, accent: 'text-amber-700 bg-amber-50 border-amber-100' },
  open_question: { label: '待确认', icon: CircleHelp, accent: 'text-rose-700 bg-rose-50 border-rose-100' },
};

function lifecycleText(snapshot: LiveSummarySnapshot | null): string {
  if (!snapshot?.templateId) return '尚未启动';
  if (snapshot.finalized) return '最终核对完成';
  if (snapshot.lifecycle === 'generating') return '正在整理新字幕';
  if (snapshot.lifecycle === 'unavailable') return '在线模型不可用';
  if (snapshot.lifecycle === 'error') return '保留上一版结果';
  return '等待稳定字幕';
}

function SummaryItemCard({ item }: { item: LiveSummaryItem }) {
  const meta = KIND_META[item.kind];
  const Icon = meta.icon;
  const maxRevision = item.evidence.reduce(
    (latest, evidence) => Math.max(latest, evidence.source_revision),
    0,
  );

  return (
    <article className="rounded-xl border border-slate-200 bg-white px-4 py-3.5 shadow-[0_1px_2px_rgba(15,23,42,0.03)]">
      <div className="flex items-start gap-3">
        <span className={`mt-0.5 inline-flex shrink-0 items-center gap-1 rounded-md border px-2 py-1 text-[11px] font-semibold ${meta.accent}`}>
          <Icon className="h-3.5 w-3.5" aria-hidden="true" />
          {meta.label}
        </span>
        <div className="min-w-0 flex-1">
          <h4 className="text-sm font-semibold leading-6 text-slate-950">{item.title}</h4>
          <p className="mt-1 whitespace-pre-wrap text-sm leading-6 text-slate-600">{item.body}</p>
          {(item.owner || item.due_at) && (
            <p className="mt-2 text-xs text-slate-500">
              {item.owner && <>负责人：{item.owner}</>}
              {item.owner && item.due_at && <span className="mx-2 text-slate-300">·</span>}
              {item.due_at && <>时间：{item.due_at}</>}
            </p>
          )}
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1.5">
          {item.status === 'needs_review' && (
            <span className="rounded-full bg-amber-100 px-2.5 py-1 text-[10px] font-semibold text-amber-800">
              转写已变更，待复核
            </span>
          )}
          <span
            className="rounded-full bg-slate-100 px-2.5 py-1 font-mono text-[10px] font-medium text-slate-600"
            title="此条纪要引用的稳定字幕版本"
          >
            依据 {item.evidence.length} · r{maxRevision}
          </span>
        </div>
      </div>
    </article>
  );
}

export function LiveSummaryPanel({ meetingId }: LiveSummaryPanelProps) {
  const [snapshot, setSnapshot] = useState<LiveSummarySnapshot | null>(null);
  const [expanded, setExpanded] = useState(true);
  const [loading, setLoading] = useState(true);
  const [starting, setStarting] = useState(false);
  const [finalizing, setFinalizing] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const current = await liveSummaryService.getCurrent(meetingId);
      setSnapshot(current);
    } catch {
      setSnapshot(null);
    } finally {
      setLoading(false);
    }
  }, [meetingId]);

  useEffect(() => {
    let disposed = false;
    setLoading(true);
    void refresh();
    let unlisten: (() => void) | undefined;
    void liveSummaryService.onState((event) => {
      if (!disposed && event.snapshot.meetingId === meetingId) {
        setSnapshot(event.snapshot);
      }
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [meetingId, refresh]);

  const start = async () => {
    setStarting(true);
    try {
      const next = await liveSummaryService.startSession(meetingId);
      setSnapshot(next);
      setExpanded(true);
      if (!next.provider.available) {
        toast.warning('实时总结尚未接通在线模型', {
          description: next.provider.error?.message || '请先在设置中配置支持的在线总结模型。',
        });
      } else if (next.transcriptCursor === 0) {
        toast.info('实时总结已启动', { description: '收到稳定字幕后会自动开始整理。' });
      } else {
        toast.success('实时总结已启动');
      }
    } catch (error) {
      const message = typeof error === 'object' && error && 'message' in error
        ? String((error as { message: unknown }).message)
        : '请确认该会议已保存，并检查总结模型设置。';
      toast.error('无法启动实时总结', { description: message });
    } finally {
      setStarting(false);
    }
  };

  const finalize = async () => {
    setFinalizing(true);
    try {
      const next = await liveSummaryService.requestFinalReconcile(meetingId);
      setSnapshot(next);
      toast.info('已请求最终核对', { description: '完成后会保留一份可恢复的最终版本。' });
    } catch {
      toast.error('无法执行最终核对，请稍后重试');
    } finally {
      setFinalizing(false);
    }
  };

  const groups = useMemo(() => {
    const result = new Map<LiveSummaryItemKind, LiveSummaryItem[]>();
    for (const item of snapshot?.items ?? []) {
      if (item.status === 'retracted') continue;
      const group = result.get(item.kind) ?? [];
      group.push(item);
      result.set(item.kind, group);
    }
    return result;
  }, [snapshot?.items]);

  const started = Boolean(snapshot?.templateId);
  const generating = snapshot?.lifecycle === 'generating';

  return (
    <section className="mx-4 mt-4 overflow-hidden rounded-2xl border border-slate-200 bg-slate-50/70">
      <div className="flex flex-wrap items-center gap-3 border-b border-slate-200 bg-white px-4 py-3">
        <button
          type="button"
          onClick={() => setExpanded((value) => !value)}
          className="flex min-w-0 flex-1 items-center gap-3 rounded-lg text-left outline-none focus-visible:ring-2 focus-visible:ring-emerald-400 focus-visible:ring-offset-2"
          aria-expanded={expanded}
        >
          <span className="grid h-9 w-9 shrink-0 place-items-center rounded-xl bg-slate-950 text-emerald-300">
            <Sparkles className="h-4 w-4" aria-hidden="true" />
          </span>
          <span className="min-w-0">
            <span className="flex flex-wrap items-center gap-2">
              <span className="text-sm font-semibold text-slate-950">实时会议纪要</span>
              <span className="inline-flex items-center gap-1.5 text-xs text-slate-500">
                {generating && <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-500" />}
                {lifecycleText(snapshot)}
              </span>
            </span>
            <span className="mt-0.5 block truncate text-xs text-slate-500">
              {snapshot?.summaryRevision
                ? `第 ${snapshot.summaryRevision} 版 · ${snapshot.items.length} 条结构化内容`
                : '在阅读文档或设计稿时，也能持续查看会议结论。'}
            </span>
          </span>
          {expanded ? <ChevronUp className="ml-auto h-4 w-4 text-slate-400" /> : <ChevronDown className="ml-auto h-4 w-4 text-slate-400" />}
        </button>

        {!started ? (
          <Button
            type="button"
            size="sm"
            onClick={start}
            disabled={starting || loading}
            className="bg-emerald-600 text-white hover:bg-emerald-700"
          >
            {starting ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Play className="mr-2 h-4 w-4" />}
            启动
          </Button>
        ) : (
          <>
            <Button type="button" size="sm" variant="ghost" onClick={refresh} aria-label="刷新实时纪要">
              <RefreshCw className="h-4 w-4" />
            </Button>
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={finalize}
              disabled={finalizing || generating || snapshot?.finalized || snapshot?.transcriptCursor === 0}
            >
              {finalizing && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              最终核对
            </Button>
          </>
        )}
      </div>

      {expanded && (
        <div className="p-4">
          {loading ? (
            <div className="flex min-h-28 items-center justify-center gap-2 text-sm text-slate-500">
              <Loader2 className="h-4 w-4 animate-spin" /> 正在读取纪要…
            </div>
          ) : started && snapshot?.error && snapshot.items.length === 0 ? (
            <div className="rounded-xl border border-amber-200 bg-amber-50 px-4 py-3 text-sm leading-6 text-amber-900">
              <div className="flex items-start gap-2">
                <AlertTriangle className="mt-1 h-4 w-4 shrink-0" />
                <div>
                  <p className="font-medium">{snapshot.error.message}</p>
                  <p className="mt-1 text-xs text-amber-700">字幕和录音仍会照常保存；配置模型后重新启动此会话即可。</p>
                </div>
              </div>
            </div>
          ) : groups.size > 0 ? (
            <div className="space-y-4">
              {[...groups.entries()].map(([kind, items]) => (
                <div key={kind} className="space-y-2">
                  {items.map((item) => <SummaryItemCard key={item.item_id} item={item} />)}
                </div>
              ))}
              <div className="flex items-center gap-1.5 px-1 text-[11px] text-slate-500">
                <ShieldCheck className="h-3.5 w-3.5 text-emerald-600" />
                纪要项必须绑定当前会议的稳定字幕版本；模型原始响应不会显示或写入数据库。
              </div>
            </div>
          ) : (
            <div className="flex min-h-32 flex-col items-center justify-center rounded-xl border border-dashed border-slate-300 bg-white px-5 text-center">
              {generating ? <Loader2 className="mb-3 h-5 w-5 animate-spin text-emerald-600" /> : <ClipboardList className="mb-3 h-5 w-5 text-slate-400" />}
              <p className="text-sm font-medium text-slate-800">
                {generating ? '正在整理稳定字幕' : started ? '等待稳定字幕' : '启动后显示结构化实时纪要'}
              </p>
              <p className="mt-1 max-w-lg text-xs leading-5 text-slate-500">
                当前切片支持已有会议 ID；录音中的新会话尚未自动绑定 canonical meeting_id。
              </p>
            </div>
          )}
        </div>
      )}
    </section>
  );
}
