'use client';

import { useCallback, useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Check, Loader2, ShieldCheck, Sparkles, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import {
  liveSummaryService,
  type LiveSummaryPreferences,
} from '@/services/liveSummaryService';

interface TemplateInfo {
  id: string;
  name: string;
  description: string;
}

const MAX_PROMPT_CHARS = 8_000;

export function LiveSummarySettingsCard() {
  const [preferences, setPreferences] = useState<LiveSummaryPreferences | null>(null);
  const [templates, setTemplates] = useState<TemplateInfo[]>([]);
  const [selectedTemplate, setSelectedTemplate] = useState('standard_meeting');
  const [replacementPrompt, setReplacementPrompt] = useState('');
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [nextPreferences, availableTemplates] = await Promise.all([
        liveSummaryService.getPreferences(),
        invoke<TemplateInfo[]>('api_list_templates'),
      ]);
      setPreferences(nextPreferences);
      setSelectedTemplate(nextPreferences.templateId);
      setTemplates(availableTemplates);
    } catch {
      toast.error('无法读取实时总结设置', {
        description: '模型设置不受影响，请稍后重新打开此页面。',
      });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const selectedTemplateInfo = useMemo(
    () => templates.find((template) => template.id === selectedTemplate),
    [selectedTemplate, templates],
  );

  const save = async () => {
    const nextPrompt = replacementPrompt.trim();
    if (nextPrompt.length > MAX_PROMPT_CHARS) {
      toast.error(`自定义提示词不能超过 ${MAX_PROMPT_CHARS} 个字符`);
      return;
    }
    setSaving(true);
    try {
      const saved = await liveSummaryService.savePreferences(
        selectedTemplate,
        nextPrompt || undefined,
      );
      setPreferences(saved);
      setReplacementPrompt('');
      toast.success('实时总结设置已保存', {
        description: '新的模板和提示词会用于之后启动的实时总结会话。',
      });
    } catch {
      toast.error('无法保存实时总结设置', {
        description: '请检查模板和提示词长度后重试。',
      });
    } finally {
      setSaving(false);
    }
  };

  const deletePrompt = async () => {
    setDeleting(true);
    try {
      const saved = await liveSummaryService.deleteCustomPrompt();
      setPreferences(saved);
      setReplacementPrompt('');
      toast.success('自定义提示词已删除');
    } catch {
      toast.error('无法删除自定义提示词，请稍后重试');
    } finally {
      setDeleting(false);
    }
  };

  return (
    <section className="overflow-hidden rounded-xl border border-emerald-100 bg-white shadow-sm">
      <div className="flex flex-col gap-4 border-b border-emerald-100 bg-[linear-gradient(110deg,#f0fdf8_0%,#ffffff_62%,#f7fee7_100%)] px-6 py-5 sm:flex-row sm:items-start sm:justify-between">
        <div className="flex gap-3">
          <span className="mt-0.5 grid h-10 w-10 shrink-0 place-items-center rounded-xl bg-emerald-600 text-white shadow-sm shadow-emerald-200">
            <Sparkles className="h-5 w-5" aria-hidden="true" />
          </span>
          <div>
            <h3 className="text-lg font-semibold tracking-tight text-slate-950">实时会议纪要</h3>
            <p className="mt-1 max-w-2xl text-sm leading-6 text-slate-600">
              使用当前在线总结模型，把稳定字幕整理为议题、决策、行动项、风险和待确认问题。
            </p>
          </div>
        </div>
        <div className="inline-flex w-fit items-center gap-1.5 rounded-full border border-emerald-200 bg-white/80 px-3 py-1.5 text-xs font-medium text-emerald-800">
          <ShieldCheck className="h-3.5 w-3.5" aria-hidden="true" />
          只引用已保存的稳定字幕
        </div>
      </div>

      <div className="space-y-6 px-6 py-5">
        {loading ? (
          <div className="flex min-h-32 items-center justify-center gap-2 text-sm text-slate-500">
            <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
            正在读取设置…
          </div>
        ) : (
          <>
            <div className="grid gap-2">
              <label htmlFor="live-summary-template" className="text-sm font-semibold text-slate-900">
                默认纪要模板
              </label>
              <select
                id="live-summary-template"
                value={selectedTemplate}
                onChange={(event) => setSelectedTemplate(event.target.value)}
                className="h-10 w-full rounded-lg border border-slate-200 bg-white px-3 text-sm text-slate-900 outline-none transition focus:border-emerald-500 focus:ring-2 focus:ring-emerald-100"
              >
                {templates.map((template) => (
                  <option key={template.id} value={template.id}>
                    {template.name}
                  </option>
                ))}
              </select>
              <p className="text-xs leading-5 text-slate-500">
                {selectedTemplateInfo?.description || '选择实时纪要要重点整理的内容结构。'}
              </p>
            </div>

            <div className="grid gap-2">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <label htmlFor="live-summary-prompt" className="text-sm font-semibold text-slate-900">
                  自定义总结要求
                </label>
                {preferences?.hasCustomPrompt && (
                  <span className="inline-flex items-center gap-1 rounded-full bg-emerald-50 px-2.5 py-1 text-xs font-medium text-emerald-700">
                    <Check className="h-3.5 w-3.5" aria-hidden="true" />
                    已保存（不回显）
                  </span>
                )}
              </div>
              <Textarea
                id="live-summary-prompt"
                value={replacementPrompt}
                onChange={(event) => setReplacementPrompt(event.target.value)}
                maxLength={MAX_PROMPT_CHARS}
                rows={5}
                placeholder={preferences?.hasCustomPrompt
                  ? '已保存的提示词不会回显。仅在需要替换时输入新内容。'
                  : '例如：优先提取技术决策、负责人、截止时间和仍需验证的风险。'}
                className="min-h-28 resize-y border-slate-200 bg-slate-50/50 text-sm leading-6 focus-visible:border-emerald-500 focus-visible:ring-emerald-100"
              />
              <div className="flex items-start justify-between gap-4 text-xs leading-5 text-slate-500">
                <p>
                  提示词只写入本地设置，不会返回界面、写入纪要版本或日志。留空保存会保留原提示词。
                </p>
                <span className="shrink-0 tabular-nums">{replacementPrompt.length}/{MAX_PROMPT_CHARS}</span>
              </div>
            </div>

            <div className="flex flex-col-reverse gap-3 border-t border-slate-100 pt-5 sm:flex-row sm:items-center sm:justify-between">
              <div>
                {preferences?.hasCustomPrompt && (
                  <Button
                    type="button"
                    variant="ghost"
                    onClick={deletePrompt}
                    disabled={deleting || saving}
                    className="text-red-600 hover:bg-red-50 hover:text-red-700"
                  >
                    {deleting ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Trash2 className="mr-2 h-4 w-4" />}
                    删除已保存提示词
                  </Button>
                )}
              </div>
              <Button
                type="button"
                onClick={save}
                disabled={saving || deleting || !selectedTemplate}
                className="bg-emerald-600 text-white hover:bg-emerald-700"
              >
                {saving && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                保存实时总结设置
              </Button>
            </div>
          </>
        )}
      </div>
    </section>
  );
}
