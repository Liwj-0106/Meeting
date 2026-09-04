'use client';

import { useEffect, useMemo, useState } from 'react';
import {
  AlertTriangle,
  CheckCircle2,
  KeyRound,
  Languages,
  Loader2,
  Save,
  ShieldCheck,
  Trash2,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Switch } from '@/components/ui/switch';
import {
  liveTranslationService,
  type LiveTranslationProvider,
  type LiveTranslationSettingsInput,
  type LiveTranslationSettingsView,
} from '@/services/liveTranslationService';

const OPENAI_ENDPOINT = 'https://api.openai.com/v1';

const DEFAULT_SETTINGS: LiveTranslationSettingsView = {
  enabled: false,
  source_language: 'auto',
  target_language: 'zh-CN',
  provider: 'openai',
  model: 'gpt-4o-mini',
  endpoint: OPENAI_ENDPOINT,
  has_api_key: false,
};

type Feedback = { tone: 'success' | 'error' | 'info'; message: string };

export function TranslationSettingsCard() {
  const [settings, setSettings] = useState<LiveTranslationSettingsView>(DEFAULT_SETTINGS);
  const [apiKey, setApiKey] = useState('');
  const [loading, setLoading] = useState(true);
  const [action, setAction] = useState<'save' | 'key' | 'clear' | null>(null);
  const [feedback, setFeedback] = useState<Feedback | null>(null);

  useEffect(() => {
    let disposed = false;
    liveTranslationService.getSettings()
      .then((next) => {
        if (!disposed) setSettings(next);
      })
      .catch((error) => {
        if (!disposed) {
          setFeedback({
            tone: 'error',
            message: liveTranslationService.toFrontendError(error).message,
          });
        }
      })
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, []);

  const providerLabel = useMemo(
    () => (settings.provider === 'openai' ? 'OpenAI' : 'OpenAI 兼容服务'),
    [settings.provider],
  );

  const updateProvider = (provider: LiveTranslationProvider) => {
    setSettings((current) => ({
      ...current,
      provider,
      endpoint: provider === 'openai' ? OPENAI_ENDPOINT : current.endpoint,
    }));
    setFeedback(null);
  };

  const saveSettings = async () => {
    setAction('save');
    setFeedback(null);
    const input: LiveTranslationSettingsInput = {
      enabled: settings.enabled,
      source_language: settings.source_language,
      target_language: 'zh-CN',
      provider: settings.provider,
      model: settings.model.trim(),
      endpoint: settings.provider === 'openai' ? OPENAI_ENDPOINT : settings.endpoint.trim(),
    };
    try {
      const saved = await liveTranslationService.saveSettings(input);
      setSettings(saved);
      setFeedback({ tone: 'success', message: '双语字幕设置已保存。' });
    } catch (error) {
      setFeedback({ tone: 'error', message: liveTranslationService.toFrontendError(error).message });
    } finally {
      setAction(null);
    }
  };

  const saveApiKey = async () => {
    if (!apiKey.trim()) return;
    setAction('key');
    setFeedback(null);
    try {
      const saved = await liveTranslationService.setApiKey(apiKey);
      setSettings(saved);
      setApiKey('');
      setFeedback({ tone: 'success', message: 'API 密钥已保存；页面不会读取或回显密钥内容。' });
    } catch (error) {
      setFeedback({ tone: 'error', message: liveTranslationService.toFrontendError(error).message });
    } finally {
      setAction(null);
    }
  };

  const clearApiKey = async () => {
    setAction('clear');
    setFeedback(null);
    try {
      const saved = await liveTranslationService.clearApiKey();
      setSettings(saved);
      setApiKey('');
      setFeedback({ tone: 'success', message: '已删除实时翻译 API 密钥。' });
    } catch (error) {
      setFeedback({ tone: 'error', message: liveTranslationService.toFrontendError(error).message });
    } finally {
      setAction(null);
    }
  };

  if (loading) {
    return (
      <section className="mt-6 flex items-center justify-center rounded-2xl border border-gray-200 bg-white py-12 text-sm text-gray-500">
        <Loader2 className="mr-2 h-4 w-4 animate-spin" />
        正在读取双语字幕设置…
      </section>
    );
  }

  return (
    <section className="mt-8 overflow-hidden rounded-2xl border border-gray-200 bg-white shadow-sm">
      <div className="border-b border-gray-100 bg-gradient-to-r from-teal-50 via-white to-white px-6 py-5">
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="flex items-start gap-3">
            <span className="rounded-xl bg-teal-600 p-2.5 text-white shadow-sm">
              <Languages className="h-5 w-5" />
            </span>
            <div>
              <h2 className="text-lg font-semibold text-gray-950">双语实时字幕</h2>
              <p className="mt-1 max-w-2xl text-sm leading-6 text-gray-500">
                在悬浮字幕中先显示原文，再显示简体中文译文。翻译在 Rust 后端异步执行，不会阻塞录音和语音识别。
              </p>
            </div>
          </div>
          <div className="flex items-center gap-3 rounded-full border border-gray-200 bg-white px-3 py-2">
            <span className="text-sm font-medium text-gray-700">启用在线翻译</span>
            <Switch
              checked={settings.enabled}
              onCheckedChange={(enabled) => setSettings((current) => ({ ...current, enabled }))}
              aria-label="启用在线实时字幕翻译"
            />
          </div>
        </div>
      </div>

      <div className="space-y-6 p-6">
        <div className="grid gap-4 md:grid-cols-2">
          <div className="space-y-2">
            <Label htmlFor="translation-source-language">原文语言</Label>
            <Select
              value={settings.source_language}
              onValueChange={(source_language) => setSettings((current) => ({ ...current, source_language }))}
            >
              <SelectTrigger id="translation-source-language"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="auto">自动检测（推荐）</SelectItem>
                <SelectItem value="ja">日语</SelectItem>
                <SelectItem value="en">英语</SelectItem>
                <SelectItem value="ko">韩语</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-2">
            <Label htmlFor="translation-target-language">译文语言</Label>
            <Input id="translation-target-language" value="简体中文（zh-CN）" disabled />
          </div>
          <div className="space-y-2">
            <Label htmlFor="translation-provider">翻译服务</Label>
            <Select value={settings.provider} onValueChange={(value) => updateProvider(value as LiveTranslationProvider)}>
              <SelectTrigger id="translation-provider"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="openai">OpenAI</SelectItem>
                <SelectItem value="openai_compatible">OpenAI 兼容服务</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-2">
            <Label htmlFor="translation-model">模型</Label>
            <Input
              id="translation-model"
              value={settings.model}
              spellCheck={false}
              onChange={(event) => setSettings((current) => ({ ...current, model: event.target.value }))}
              placeholder="gpt-4o-mini"
            />
          </div>
        </div>

        <div className="space-y-2">
          <Label htmlFor="translation-endpoint">服务地址</Label>
          <Input
            id="translation-endpoint"
            value={settings.provider === 'openai' ? OPENAI_ENDPOINT : settings.endpoint}
            disabled={settings.provider === 'openai'}
            spellCheck={false}
            onChange={(event) => setSettings((current) => ({ ...current, endpoint: event.target.value }))}
            placeholder="https://example.com/v1"
          />
          <p className="text-xs leading-5 text-gray-500">
            远程服务只允许 HTTPS；HTTP 仅允许本机回环地址。地址不得携带用户名、密码、查询参数或片段。
          </p>
        </div>

        <div className="rounded-xl border border-gray-200 bg-gray-50 p-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-2">
              <KeyRound className="h-4 w-4 text-gray-600" />
              <span className="font-medium text-gray-900">{providerLabel} API 密钥</span>
              <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${settings.has_api_key
                ? 'bg-emerald-100 text-emerald-700'
                : 'bg-amber-100 text-amber-700'}`}
              >
                {settings.has_api_key ? '已配置' : '未配置'}
              </span>
            </div>
          </div>
          <div className="mt-3 flex flex-col gap-2 sm:flex-row">
            <Input
              type="password"
              value={apiKey}
              autoComplete="new-password"
              spellCheck={false}
              onChange={(event) => setApiKey(event.target.value)}
              placeholder={settings.has_api_key ? '已保存；输入新值可替换' : '输入 API 密钥'}
              aria-label="新的实时翻译 API 密钥"
            />
            <Button type="button" onClick={() => void saveApiKey()} disabled={!apiKey.trim() || action !== null}>
              {action === 'key' && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              保存密钥
            </Button>
            {settings.has_api_key && (
              <Button type="button" variant="outline" onClick={() => void clearApiKey()} disabled={action !== null}>
                {action === 'clear' ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Trash2 className="mr-2 h-4 w-4" />}
                删除
              </Button>
            )}
          </div>
          <p className="mt-2 text-xs leading-5 text-gray-500">
            密钥只由 Rust 后端读取，界面只能看到“是否已配置”。当前保存在本机 Meetily 数据库中，尚未接入系统凭据库。
          </p>
        </div>

        <div className="grid gap-3 md:grid-cols-2">
          <div className="flex gap-3 rounded-xl border border-teal-100 bg-teal-50/70 p-4 text-sm leading-6 text-teal-950">
            <ShieldCheck className="mt-0.5 h-5 w-5 shrink-0 text-teal-700" />
            <p>仅在你启用翻译且悬浮字幕处于显示状态时，当前字幕文本才会发送到所选服务。</p>
          </div>
          <div className="flex gap-3 rounded-xl border border-amber-100 bg-amber-50/70 p-4 text-sm leading-6 text-amber-950">
            <AlertTriangle className="mt-0.5 h-5 w-5 shrink-0 text-amber-700" />
            <p>在线服务可能计费并保留请求日志；隐私与数据期限取决于你选择的提供商。</p>
          </div>
        </div>

        {settings.enabled && !settings.has_api_key && (
          <div className="flex gap-2 rounded-lg border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-800">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
            启用后还需要保存 API 密钥，悬浮字幕才能获得译文。
          </div>
        )}

        {feedback && (
          <div className={`flex gap-2 rounded-lg border px-4 py-3 text-sm ${feedback.tone === 'success'
            ? 'border-emerald-200 bg-emerald-50 text-emerald-800'
            : feedback.tone === 'error'
              ? 'border-red-200 bg-red-50 text-red-800'
              : 'border-blue-200 bg-blue-50 text-blue-800'}`}
          >
            {feedback.tone === 'success'
              ? <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0" />
              : <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />}
            <span>{feedback.message}</span>
          </div>
        )}

        <div className="flex justify-end">
          <Button type="button" onClick={() => void saveSettings()} disabled={action !== null}>
            {action === 'save' ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
            保存双语字幕设置
          </Button>
        </div>
      </div>
    </section>
  );
}
