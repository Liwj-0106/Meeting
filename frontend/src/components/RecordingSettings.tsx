import React, { useState, useEffect, useRef } from 'react';
import { Switch } from '@/components/ui/switch';
import { FolderOpen } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { DeviceSelection, DISABLED_AUDIO_DEVICE } from '@/components/DeviceSelection';
import type { SelectedDevices } from '@/components/DeviceSelection';
import Analytics from '@/lib/analytics';
import { toast } from 'sonner';
import { loadAppStore } from '@/lib/appStore';
import { useConfig } from '@/contexts/ConfigContext';
import { configService, type RecordingPreferences } from '@/services/configService';
import { RecordingPreferencesSaveError } from '@/lib/recording-preferences';

export type { RecordingPreferences } from '@/services/configService';

const DEFAULT_RECORDING_PREFERENCES: RecordingPreferences = {
  save_folder: '',
  auto_save: true,
  file_format: 'mp4',
  preferred_mic_device: null,
  preferred_system_device: null,
};

interface RecordingSettingsProps {
  onSave?: (preferences: RecordingPreferences) => void;
}

export function RecordingSettings({ onSave }: RecordingSettingsProps) {
  const { saveRecordingPreferences } = useConfig();
  const [preferences, setPreferences] = useState<RecordingPreferences>(
    DEFAULT_RECORDING_PREFERENCES,
  );
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [showRecordingNotification, setShowRecordingNotification] = useState(true);
  const committedPreferencesRef = useRef<RecordingPreferences>(
    DEFAULT_RECORDING_PREFERENCES,
  );
  const saveRevisionRef = useRef(0);

  // Load recording preferences on component mount
  useEffect(() => {
    const loadPreferences = async () => {
      try {
        const prefs = await configService.getRecordingPreferences();
        committedPreferencesRef.current = prefs;
        setPreferences(prefs);
      } catch (error) {
        console.error('无法加载录音偏好：', error);
        // If loading fails, get default folder path
        try {
          const defaultPath = await invoke<string>('get_default_recordings_folder_path');
          const fallback = {
            ...DEFAULT_RECORDING_PREFERENCES,
            save_folder: defaultPath,
          };
          committedPreferencesRef.current = fallback;
          setPreferences(fallback);
        } catch (defaultError) {
          console.error('无法获取默认录音目录：', defaultError);
        }
      } finally {
        setLoading(false);
      }
    };

    loadPreferences();
  }, []);

  // Load recording notification preference
  useEffect(() => {
    const loadNotificationPref = async () => {
      try {
        const store = await loadAppStore('preferences.json');
        const show = await store.get<boolean>('show_recording_notification') ?? true;
        setShowRecordingNotification(show);
      } catch (error) {
        console.error('Failed to load notification preference:', error);
      }
    };
    loadNotificationPref();
  }, []);

  const handleAutoSaveToggle = async (enabled: boolean) => {
    const newPreferences = { ...preferences, auto_save: enabled };
    setPreferences(newPreferences);
    const saved = await persistPreferences(newPreferences, {
      title: '录音保存设置已更新',
    });

    if (saved) {
      await Analytics.track('auto_save_recording_toggled', {
        enabled: saved.auto_save.toString()
      });
    }
  };

  const handleDeviceChange = async (devices: SelectedDevices) => {
    const newPreferences = {
      ...preferences,
      preferred_mic_device: devices.micDevice,
      preferred_system_device: devices.systemDevice
    };
    setPreferences(newPreferences);
    const saved = await persistPreferences(newPreferences, {
      title: '音频设备已保存',
      includeDevices: true,
    });

    if (saved) {
      // Individual selections are also tracked by DeviceSelection.
      await Analytics.track('default_devices_changed', {
        has_preferred_microphone: (!!saved.preferred_mic_device).toString(),
        has_preferred_system_audio: (!!saved.preferred_system_device).toString()
      });
    }
  };

  const handleOpenFolder = async () => {
    try {
      await invoke('open_recordings_folder');
    } catch (error) {
      console.error('Failed to open recordings folder:', error);
    }
  };

  const handleNotificationToggle = async (enabled: boolean) => {
    const previous = showRecordingNotification;
    try {
      setShowRecordingNotification(enabled);
      const store = await loadAppStore('preferences.json');
      await store.set('show_recording_notification', enabled);
      await store.save();
      toast.success('提醒设置已保存');
      await Analytics.track('recording_notification_preference_changed', {
        enabled: enabled.toString()
      });
    } catch (error) {
      setShowRecordingNotification(previous);
      console.error('无法保存提醒设置：', error);
      toast.error('提醒设置保存失败');
    }
  };

  const persistPreferences = async (
    prefs: RecordingPreferences,
    message: { title: string; includeDevices?: boolean },
  ): Promise<RecordingPreferences | null> => {
    const revision = ++saveRevisionRef.current;
    setSaving(true);
    try {
      const saved = await saveRecordingPreferences(prefs);
      if (revision !== saveRevisionRef.current) {
        return null;
      }

      committedPreferencesRef.current = saved;
      setPreferences(saved);
      onSave?.(saved);

      const describeDevice = (device: string | null) => {
        if (device === null) return '系统默认';
        if (device.toLowerCase() === DISABLED_AUDIO_DEVICE) return '已关闭';
        return device;
      };
      toast.success(message.title, message.includeDevices ? {
        description: `麦克风：${describeDevice(saved.preferred_mic_device)}；系统音频：${describeDevice(saved.preferred_system_device)}`,
      } : undefined);
      return saved;
    } catch (error) {
      if (revision === saveRevisionRef.current) {
        const recovered = error instanceof RecordingPreferencesSaveError
          && error.recoveredPreferences
          ? error.recoveredPreferences as RecordingPreferences
          : committedPreferencesRef.current;
        committedPreferencesRef.current = recovered;
        setPreferences(recovered);
        console.error('无法保存录音偏好：', error);
        toast.error('设置保存失败，已恢复上次保存的值', {
          description: error instanceof Error ? error.message : String(error)
        });
      }
      return null;
    } finally {
      if (revision === saveRevisionRef.current) {
        setSaving(false);
      }
    }
  };

  if (loading) {
    return (
      <div className="animate-pulse">
        <div className="h-4 bg-gray-200 rounded w-1/4 mb-4"></div>
        <div className="h-8 bg-gray-200 rounded mb-4"></div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div>
        <h3 className="text-lg font-semibold mb-4">录音设置</h3>
        <p className="text-sm text-gray-600 mb-6">
          配置会议录音的保存方式和下一次录音使用的音频设备。
        </p>
      </div>

      {/* Auto Save Toggle */}
      <div className="flex items-center justify-between p-4 border rounded-lg">
        <div className="flex-1">
          <div className="font-medium">保存会议录音</div>
          <div className="text-sm text-gray-600">
            录音停止后自动保存音频文件
          </div>
        </div>
        <Switch
          checked={preferences.auto_save}
          onCheckedChange={handleAutoSaveToggle}
          disabled={saving}
        />
      </div>

      {/* Folder Location - Only shown when auto_save is enabled */}
      {preferences.auto_save && (
        <div className="space-y-4">
          <div className="p-4 border rounded-lg bg-gray-50">
            <div className="font-medium mb-2">保存位置</div>
            <div className="text-sm text-gray-600 mb-3 break-all">
              {preferences.save_folder || '默认目录'}
            </div>
            <button
              onClick={handleOpenFolder}
              className="flex items-center gap-2 px-3 py-2 text-sm border border-gray-300 rounded-md hover:bg-gray-50 transition-colors"
            >
              <FolderOpen className="w-4 h-4" />
              打开文件夹
            </button>
          </div>

          <div className="p-4 border rounded-lg bg-blue-50">
            <div className="text-sm text-blue-800">
              <strong>文件格式：</strong>{preferences.file_format.toUpperCase()}
            </div>
            <div className="text-xs text-blue-600 mt-1">
              文件名包含时间戳：recording_YYYYMMDD_HHMMSS.{preferences.file_format}
            </div>
          </div>
        </div>
      )}

      {/* Info when auto_save is disabled */}
      {!preferences.auto_save && (
        <div className="p-4 border rounded-lg bg-yellow-50">
          <div className="text-sm text-yellow-800">
            当前不会保存会议音频。开启“保存会议录音”后，停止录音时会自动写入文件。
          </div>
        </div>
      )}

      {/* Recording Notification Toggle */}
      <div className="flex items-center justify-between p-4 border rounded-lg">
        <div className="flex-1">
          <div className="font-medium">录音开始提醒</div>
          <div className="text-sm text-gray-600">
            录音开始时提醒你告知参会者
          </div>
        </div>
        <Switch
          checked={showRecordingNotification}
          onCheckedChange={handleNotificationToggle}
        />
      </div>

      {/* Device Preferences */}
      <div className="space-y-4">
        <div className="border-t pt-6">
          <h4 className="text-base font-medium text-gray-900 mb-4">默认音频设备</h4>
          <p className="text-sm text-gray-600 mb-4">
            保存后立即用于本次应用会话中的下一次录音，无需重启。设备断开时会保留你的选择并标为不可用。
          </p>

          <div className="border rounded-lg p-4 bg-gray-50">
            <DeviceSelection
              selectedDevices={{
                micDevice: preferences.preferred_mic_device,
                systemDevice: preferences.preferred_system_device
              }}
              onDeviceChange={handleDeviceChange}
              disabled={saving}
            />
          </div>
        </div>
      </div>
    </div>
  );
}
