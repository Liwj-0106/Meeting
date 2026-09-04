'use client';

import { useEffect, useMemo, useState } from 'react';
import {
  AlertTriangle,
  CheckCircle2,
  Cloud,
  Cpu,
  KeyRound,
  Loader2,
  Radio,
  Save,
  ShieldCheck,
  Trash2,
  Wifi,
} from 'lucide-react';
import { Button } from './ui/button';
import { Input } from './ui/input';
import { Label } from './ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from './ui/select';
import { Switch } from './ui/switch';
import { ModelManager } from './WhisperModelManager';
import { ParakeetModelManager } from './ParakeetModelManager';
import { configService } from '@/services/configService';
import {
  LOCAL_TRANSCRIPT_DEFAULTS,
  configuredKeyForProvider,
  isStreamingTranscriptProvider,
  normalizeTranscriptConfig,
  type StreamingLatencyMode,
  type StreamingProviderConfig,
  type TranscriptModelConfig,
  type TranscriptProvider,
} from '@/types/transcription-config';

/** Compatibility alias for older callers. This public view never contains a secret. */
export type TranscriptModelProps = TranscriptModelConfig;

export interface TranscriptSettingsProps {
  transcriptModelConfig: TranscriptModelConfig;
  setTranscriptModelConfig: (config: TranscriptModelConfig) => void;
  onModelSelect?: () => void;
}

type StreamingProvider = Extract<TranscriptProvider, 'deepgram' | 'openai'>;
type Feedback = { tone: 'success' | 'error' | 'info'; message: string } | null;

const PROVIDERS: Array<{
  id: TranscriptProvider;
  title: string;
  description: string;
  badge: string;
}> = [
  {
    id: 'parakeet',
    title: '本地 Parakeet',
    description: '离线运行，速度优先，适合资源受限设备。',
    badge: '本地',
  },
  {
    id: 'localWhisper',
    title: '本地 Whisper',
    description: '离线多语言识别，适合中文、日语与视频字幕。',
    badge: '本地',
  },
  {
    id: 'deepgram',
    title: 'Deepgram 流式转写',
    description: '低延迟增量结果，可返回词级时间与说话人标签。',
    badge: '在线',
  },
  {
    id: 'openai',
    title: 'OpenAI Realtime',
    description: '实时多语言转写，支持提示词和关键词上下文。',
    badge: '在线',
  },
];

const STREAMING_MODELS: Record<StreamingProvider, Array<{ value: string; label: string }>> = {
  deepgram: [
    { value: 'nova-3', label: 'Nova-3（推荐）' },
    { value: 'nova-2', label: 'Nova-2' },
  ],
  openai: [
    { value: 'gpt-live-transcribe', label: 'gpt-live-transcribe（推荐）' },
    { value: 'gpt-4o-transcribe', label: 'gpt-4o-transcribe' },
    { value: 'gpt-4o-mini-transcribe', label: 'gpt-4o-mini-transcribe' },
  ],
};

const LANGUAGES = [
  { value: 'auto', label: '自动检测' },
  { value: 'zh', label: '中文' },
  { value: 'ja', label: '日语' },
  { value: 'en', label: '英语' },
  { value: 'ko', label: '韩语' },
];

const LATENCY_MODES: Array<{ value: StreamingLatencyMode; label: string; hint: string }> = [
  { value: 'minimal', label: '最低延迟', hint: '更快出现字幕，可能产生更多修订' },
  { value: 'low', label: '低延迟', hint: '适合实时字幕' },
  { value: 'balanced', label: '平衡', hint: '会议场景推荐' },
  { value: 'high', label: '高稳定', hint: '等待更久，减少字幕跳动' },
];

function providerDefaultModel(provider: TranscriptProvider, config: TranscriptModelConfig): string {
  if (provider === 'deepgram' || provider === 'openai') {
    return config.streamingConfig.providers[provider].model;
  }
  return LOCAL_TRANSCRIPT_DEFAULTS[provider];
}

export function TranscriptSettings({
  transcriptModelConfig,
  setTranscriptModelConfig,
  onModelSelect,
}: TranscriptSettingsProps) {
  const [draft, setDraft] = useState(() => normalizeTranscriptConfig(transcriptModelConfig));
  const [apiKeyDraft, setApiKeyDraft] = useState('');
  const [feedback, setFeedback] = useState<Feedback>(null);
  const [saving, setSaving] = useState(false);
  const [keyAction, setKeyAction] = useState<'save' | 'delete' | 'test' | null>(null);

  useEffect(() => {
    setDraft(normalizeTranscriptConfig(transcriptModelConfig));
  }, [transcriptModelConfig]);

  const streamingProvider = isStreamingTranscriptProvider(draft.provider)
    ? draft.provider
    : null;
  const streamingConfig = streamingProvider
    ? draft.streamingConfig.providers[streamingProvider]
    : null;
  const keyConfigured = streamingProvider
    ? configuredKeyForProvider(draft, streamingProvider)
    : false;

  const latencyHint = useMemo(
    () => LATENCY_MODES.find((item) => item.value === streamingConfig?.latencyMode)?.hint,
    [streamingConfig?.latencyMode],
  );

  const selectProvider = (provider: TranscriptProvider) => {
    setFeedback(null);
    setApiKeyDraft('');
    setDraft((current) => ({
      ...current,
      provider,
      model: providerDefaultModel(provider, current),
      hasApiKey: provider === 'deepgram' || provider === 'openai'
        ? configuredKeyForProvider(current, provider)
        : false,
    }));
  };

  const updateStreamingProvider = (patch: Partial<StreamingProviderConfig>) => {
    if (!streamingProvider) return;
    setDraft((current) => {
      const updatedProvider = {
        ...current.streamingConfig.providers[streamingProvider],
        ...patch,
      };
      return {
        ...current,
        model: patch.model ?? current.model,
        streamingConfig: {
          ...current.streamingConfig,
          providers: {
            ...current.streamingConfig.providers,
            [streamingProvider]: updatedProvider,
          },
        },
      };
    });
  };

  const persistConfig = async (next = draft, closeAfterSave = false) => {
    const normalized = normalizeTranscriptConfig(next);
    setSaving(true);
    setFeedback(null);
    try {
      await configService.saveTranscriptConfig(normalized);
      setDraft(normalized);
      setTranscriptModelConfig(normalized);
      setFeedback({ tone: 'success', message: '转写设置已保存，将从下一次录音开始生效。' });
      if (closeAfterSave) onModelSelect?.();
    } catch (error) {
      setFeedback({
        tone: 'error',
        message: `保存失败：${error instanceof Error ? error.message : String(error)}`,
      });
    } finally {
      setSaving(false);
    }
  };

  const persistLocalModel = (provider: 'parakeet' | 'localWhisper', model: string) => {
    const next = normalizeTranscriptConfig({ ...draft, provider, model, hasApiKey: false });
    setDraft(next);
    void persistConfig(next, true);
  };

  const updateKeyStatus = (provider: StreamingProvider, configured: boolean) => {
    const next = {
      ...draft,
      apiKeyConfigured: {
        deepgram: draft.apiKeyConfigured?.deepgram ?? false,
        openai: draft.apiKeyConfigured?.openai ?? false,
        [provider]: configured,
      },
      hasApiKey: draft.provider === provider ? configured : draft.hasApiKey,
    };
    setDraft(next);
    setTranscriptModelConfig(next);
  };

  const saveApiKey = async () => {
    if (!streamingProvider || !apiKeyDraft.trim()) {
      setFeedback({ tone: 'error', message: '请输入新的 API 密钥。' });
      return;
    }
    setKeyAction('save');
    setFeedback(null);
    try {
      await configService.setTranscriptApiKey(streamingProvider, apiKeyDraft.trim());
      updateKeyStatus(streamingProvider, true);
      setApiKeyDraft('');
      setFeedback({ tone: 'success', message: '密钥已安全保存；客户端不会回读或显示密钥正文。' });
    } catch (error) {
      setFeedback({ tone: 'error', message: `密钥保存失败：${error instanceof Error ? error.message : String(error)}` });
    } finally {
      setKeyAction(null);
    }
  };

  const deleteApiKey = async () => {
    if (!streamingProvider) return;
    setKeyAction('delete');
    setFeedback(null);
    try {
      await configService.deleteTranscriptApiKey(streamingProvider);
      updateKeyStatus(streamingProvider, false);
      setApiKeyDraft('');
      setFeedback({ tone: 'success', message: '已删除该服务的密钥。' });
    } catch (error) {
      setFeedback({ tone: 'error', message: `删除失败：${error instanceof Error ? error.message : String(error)}` });
    } finally {
      setKeyAction(null);
    }
  };

  const testConnection = async () => {
    if (!streamingProvider) return;
    const candidate = apiKeyDraft.trim();
    if (!candidate && !keyConfigured) {
      setFeedback({ tone: 'error', message: '请先输入或保存 API 密钥。' });
      return;
    }
    setKeyAction('test');
    setFeedback(null);
    try {
      const result = await configService.testTranscriptConnection({
        provider: streamingProvider,
        apiKey: candidate || undefined,
        useStoredKey: !candidate,
        configOverride: normalizeTranscriptConfig(draft),
      });
      setFeedback({
        tone: result.ok ? 'success' : 'error',
        message: result.latencyMs
          ? `${result.message}（${result.latencyMs} ms）`
          : result.message,
      });
    } catch (error) {
      setFeedback({ tone: 'error', message: `连接测试失败：${error instanceof Error ? error.message : String(error)}` });
    } finally {
      setKeyAction(null);
    }
  };

  return (
    <div className="mx-auto w-full max-w-5xl space-y-6 py-2">
      <section className="space-y-3">
        <div>
          <h2 className="text-lg font-semibold text-gray-950">语音转写引擎</h2>
          <p className="mt-1 text-sm text-gray-500">本地模型保护隐私；在线流式模型优先保证字幕实时性。</p>
        </div>
        <div className="grid gap-3 md:grid-cols-2">
          {PROVIDERS.map((provider) => {
            const selected = draft.provider === provider.id;
            const isOnline = provider.id === 'deepgram' || provider.id === 'openai';
            return (
              <button
                key={provider.id}
                type="button"
                onClick={() => selectProvider(provider.id)}
                className={`group rounded-xl border p-4 text-left transition-colors ${selected
                  ? 'border-blue-500 bg-blue-50 ring-2 ring-blue-100'
                  : 'border-gray-200 bg-white hover:border-gray-300 hover:bg-gray-50'}`}
              >
                <div className="flex items-start gap-3">
                  <span className={`rounded-lg p-2 ${selected ? 'bg-blue-600 text-white' : 'bg-gray-100 text-gray-600'}`}>
                    {isOnline ? <Cloud className="h-5 w-5" /> : <Cpu className="h-5 w-5" />}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="flex items-center justify-between gap-2">
                      <span className="font-medium text-gray-950">{provider.title}</span>
                      <span className={`rounded-full px-2 py-0.5 text-xs ${isOnline ? 'bg-violet-100 text-violet-700' : 'bg-emerald-100 text-emerald-700'}`}>
                        {provider.badge}
                      </span>
                    </span>
                    <span className="mt-1 block text-sm leading-5 text-gray-500">{provider.description}</span>
                  </span>
                </div>
              </button>
            );
          })}
        </div>
      </section>

      {draft.provider === 'parakeet' && (
        <section className="rounded-xl border border-gray-200 bg-white p-5">
          <h3 className="mb-1 font-semibold text-gray-950">Parakeet 模型</h3>
          <p className="mb-4 text-sm text-gray-500">模型文件保存在 Meetily 的 D 盘便携目录。</p>
          <ParakeetModelManager
            selectedModel={draft.model}
            autoSave={false}
            onModelSelect={(model) => persistLocalModel('parakeet', model)}
          />
        </section>
      )}

      {draft.provider === 'localWhisper' && (
        <section className="rounded-xl border border-gray-200 bg-white p-5">
          <h3 className="mb-1 font-semibold text-gray-950">Whisper 模型</h3>
          <p className="mb-4 text-sm text-gray-500">中文、日语等多语言视频建议从 Small 或更高规格开始。</p>
          <ModelManager
            selectedModel={draft.model}
            autoSave={false}
            onModelSelect={(model) => persistLocalModel('localWhisper', model)}
          />
        </section>
      )}

      {streamingProvider && streamingConfig && (
        <>
          <section className="space-y-5 rounded-xl border border-gray-200 bg-white p-5">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <h3 className="font-semibold text-gray-950">流式识别</h3>
                <p className="mt-1 text-sm text-gray-500">中间结果会持续修订，稳定结果才进入会议纪要。</p>
              </div>
              <span className={`inline-flex items-center gap-1.5 rounded-full px-3 py-1 text-xs font-medium ${keyConfigured
                ? 'bg-emerald-100 text-emerald-700'
                : 'bg-amber-100 text-amber-700'}`}
              >
                {keyConfigured ? <ShieldCheck className="h-3.5 w-3.5" /> : <AlertTriangle className="h-3.5 w-3.5" />}
                {keyConfigured ? '密钥已配置' : '密钥未配置'}
              </span>
            </div>

            <div className="grid gap-4 md:grid-cols-2">
              <div className="space-y-2">
                <Label htmlFor="streaming-model">模型</Label>
                <Select
                  value={streamingConfig.model}
                  onValueChange={(model) => updateStreamingProvider({ model })}
                >
                  <SelectTrigger id="streaming-model"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    {STREAMING_MODELS[streamingProvider].map((model) => (
                      <SelectItem key={model.value} value={model.value}>{model.label}</SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-2">
                <Label htmlFor="streaming-language">主要语言</Label>
                <Select
                  value={streamingConfig.language}
                  onValueChange={(language) => updateStreamingProvider({ language })}
                >
                  <SelectTrigger id="streaming-language"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    {LANGUAGES.map((language) => (
                      <SelectItem key={language.value} value={language.value}>{language.label}</SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-2">
                <Label htmlFor="streaming-latency">实时性</Label>
                <Select
                  value={streamingConfig.latencyMode}
                  onValueChange={(value) => updateStreamingProvider({ latencyMode: value as StreamingLatencyMode })}
                >
                  <SelectTrigger id="streaming-latency"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    {LATENCY_MODES.map((mode) => (
                      <SelectItem key={mode.value} value={mode.value}>{mode.label}</SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <p className="text-xs text-gray-500">{latencyHint}</p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="streaming-keywords">专业词与人名</Label>
                <Input
                  id="streaming-keywords"
                  value={streamingConfig.keywords.join(', ')}
                  onChange={(event) => updateStreamingProvider({
                    keywords: event.target.value.split(/[,，\n]/).map((word) => word.trim()).filter(Boolean),
                  })}
                  placeholder="Meetily, WebRTC, 项目代号"
                />
                <p className="text-xs text-gray-500">用逗号分隔，最多数量和长度由后端校验。</p>
              </div>
            </div>

            {streamingProvider === 'deepgram' ? (
              <div className="flex items-center justify-between gap-4 rounded-lg bg-gray-50 p-4">
                <div>
                  <Label htmlFor="streaming-diarization">在线说话人识别</Label>
                  <p className="mt-1 text-xs text-gray-500">返回 Speaker 0、Speaker 1 等标签；身份命名需会后确认。</p>
                </div>
                <Switch
                  id="streaming-diarization"
                  checked={streamingConfig.diarization}
                  onCheckedChange={(diarization) => updateStreamingProvider({ diarization })}
                />
              </div>
            ) : (
              <div className="rounded-lg border border-amber-200 bg-amber-50 p-4 text-sm text-amber-900">
                OpenAI Realtime 当前不会直接返回可靠的说话人标签、词级时间或置信度；Meetily 不会伪造这些字段。
              </div>
            )}
          </section>

          <section className="space-y-4 rounded-xl border border-gray-200 bg-white p-5">
            <div className="flex items-center gap-2">
              <KeyRound className="h-5 w-5 text-gray-600" />
              <h3 className="font-semibold text-gray-950">API 密钥</h3>
            </div>
            <div className="space-y-2">
              <Label htmlFor="transcript-api-key">新密钥</Label>
              <Input
                id="transcript-api-key"
                type="password"
                value={apiKeyDraft}
                autoComplete="new-password"
                spellCheck={false}
                onChange={(event) => setApiKeyDraft(event.target.value)}
                placeholder={keyConfigured ? '已保存；输入新值可替换' : '输入 API 密钥'}
              />
              <p className="text-xs text-gray-500">保存后的密钥只由 Rust 后端读取，Web 界面仅能看到配置状态。</p>
            </div>
            <div className="flex flex-wrap gap-2">
              <Button type="button" onClick={() => void saveApiKey()} disabled={keyAction !== null || !apiKeyDraft.trim()}>
                {keyAction === 'save' && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                保存新密钥
              </Button>
              <Button type="button" variant="outline" onClick={() => void testConnection()} disabled={keyAction !== null || (!apiKeyDraft.trim() && !keyConfigured)}>
                {keyAction === 'test' ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Wifi className="mr-2 h-4 w-4" />}
                测试连接
              </Button>
              {keyConfigured && (
                <Button type="button" variant="outline" className="text-red-600 hover:text-red-700" onClick={() => void deleteApiKey()} disabled={keyAction !== null}>
                  {keyAction === 'delete' ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Trash2 className="mr-2 h-4 w-4" />}
                  删除密钥
                </Button>
              )}
            </div>
          </section>

          <section className="space-y-4 rounded-xl border border-gray-200 bg-white p-5">
            <div className="flex items-center justify-between gap-4">
              <div>
                <div className="flex items-center gap-2">
                  <Radio className="h-5 w-5 text-gray-600" />
                  <h3 className="font-semibold text-gray-950">断线时本地回退（开发中）</h3>
                </div>
                <p className="mt-1 text-sm text-gray-500">
                  当前会保留录音并自动重连；本地模型热切换将在下一阶段接通，暂不承诺已经回退。
                </p>
              </div>
              <Switch
                checked={false}
                disabled
                aria-label="本地回退正在开发中"
                onCheckedChange={(enabled) => setDraft((current) => ({
                  ...current,
                  streamingConfig: {
                    ...current.streamingConfig,
                    fallback: { ...current.streamingConfig.fallback, enabled },
                  },
                }))}
              />
            </div>
            {draft.streamingConfig.fallback.enabled && (
              <div className="grid gap-4 md:grid-cols-2">
                <div className="space-y-2">
                  <Label>本地引擎</Label>
                  <Select
                    value={draft.streamingConfig.fallback.provider}
                    disabled
                    onValueChange={(value) => setDraft((current) => ({
                      ...current,
                      streamingConfig: {
                        ...current.streamingConfig,
                        fallback: {
                          ...current.streamingConfig.fallback,
                          provider: value as 'parakeet' | 'localWhisper',
                          model: LOCAL_TRANSCRIPT_DEFAULTS[value as 'parakeet' | 'localWhisper'],
                        },
                      },
                    }))}
                  >
                    <SelectTrigger><SelectValue /></SelectTrigger>
                    <SelectContent>
                      <SelectItem value="parakeet">Parakeet</SelectItem>
                      <SelectItem value="localWhisper">Whisper</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
                <div className="space-y-2">
                  <Label htmlFor="fallback-model">回退模型</Label>
                  <Input
                    id="fallback-model"
                    value={draft.streamingConfig.fallback.model}
                    disabled
                    onChange={(event) => setDraft((current) => ({
                      ...current,
                      streamingConfig: {
                        ...current.streamingConfig,
                        fallback: { ...current.streamingConfig.fallback, model: event.target.value },
                      },
                    }))}
                  />
                </div>
              </div>
            )}
          </section>

          <details className="rounded-xl border border-gray-200 bg-white p-5">
            <summary className="cursor-pointer font-medium text-gray-950">高级连接设置</summary>
            <div className="mt-4 space-y-2">
              <Label htmlFor="endpoint-override">自定义 WebSocket 地址</Label>
              <Input
                id="endpoint-override"
                value={streamingConfig.endpointOverride ?? ''}
                onChange={(event) => updateStreamingProvider({ endpointOverride: event.target.value.trim() || null })}
                placeholder={streamingProvider === 'deepgram'
                  ? 'wss://api.deepgram.com/v1/listen'
                  : 'wss://api.openai.com/v1/realtime'}
              />
              <p className="text-xs text-gray-500">仅允许 wss://；地址不得包含用户名、密码或密钥查询参数。</p>
            </div>
          </details>
        </>
      )}

      {feedback && (
        <div className={`flex items-start gap-2 rounded-lg border px-4 py-3 text-sm ${feedback.tone === 'success'
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

      <div className="sticky bottom-0 flex items-center justify-between gap-4 rounded-xl border border-gray-200 bg-white/95 p-4 shadow-lg backdrop-blur">
        <p className="text-sm text-gray-500">录音进行中不会热切换引擎，避免一场会议混用配置。</p>
        <Button type="button" onClick={() => void persistConfig()} disabled={saving}>
          {saving ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
          保存转写设置
        </Button>
      </div>
    </div>
  );
}
