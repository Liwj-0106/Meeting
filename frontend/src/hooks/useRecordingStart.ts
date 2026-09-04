import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTranscripts } from '@/contexts/TranscriptContext';
import { useSidebar } from '@/components/Sidebar/SidebarProvider';
import { useConfig } from '@/contexts/ConfigContext';
import { useRecordingState, RecordingStatus } from '@/contexts/RecordingStateContext';
import { recordingService } from '@/services/recordingService';
import { liveSummaryService } from '@/services/liveSummaryService';
import Analytics from '@/lib/analytics';
import { showRecordingNotification } from '@/lib/recordingNotification';
import { toast } from 'sonner';
import type { ModelInfo as WhisperModelInfo } from '@/lib/whisper';
import type { ParakeetModelInfo } from '@/lib/parakeet';
import {
  configuredKeyForProvider,
  isStreamingTranscriptProvider,
} from '@/types/transcription-config';

interface UseRecordingStartReturn {
  handleRecordingStart: () => Promise<void>;
  isAutoStarting: boolean;
}

type RecordingStartSource = 'home_page' | 'sidebar_auto' | 'sidebar_direct';
type LocalTranscriptionModel = WhisperModelInfo | ParakeetModelInfo;
type TranscriptionReadiness =
  | 'ready'
  | 'downloading'
  | 'missing_local_model'
  | 'missing_api_key';

/**
 * Custom hook for managing recording start lifecycle.
 * Handles both manual start (button click) and auto-start (from sidebar navigation).
 *
 * Features:
 * - Meeting title generation (format: Meeting DD_MM_YY_HH_MM_SS)
 * - Transcript clearing on start
 * - Analytics tracking
 * - Recording notification display
 * - Auto-start from sidebar via sessionStorage flag
 */
export function useRecordingStart(
  isRecording: boolean,
  setIsRecording: (value: boolean) => void,
  showModal?: (name: 'modelSelector', message?: string) => void
): UseRecordingStartReturn {
  const [isAutoStarting, setIsAutoStarting] = useState(false);

  const { clearTranscripts, setMeetingTitle } = useTranscripts();
  const { setIsMeetingActive } = useSidebar();
  const { selectedDevices, transcriptModelConfig } = useConfig();
  const { setStatus } = useRecordingState();

  // Generate meeting title with timestamp
  const generateMeetingTitle = useCallback(() => {
    const now = new Date();
    const day = String(now.getDate()).padStart(2, '0');
    const month = String(now.getMonth() + 1).padStart(2, '0');
    const year = String(now.getFullYear()).slice(-2);
    const hours = String(now.getHours()).padStart(2, '0');
    const minutes = String(now.getMinutes()).padStart(2, '0');
    const seconds = String(now.getSeconds()).padStart(2, '0');
    return `Meeting ${day}_${month}_${year}_${hours}_${minutes}_${seconds}`;
  }, []);

  // Streaming providers do not have a downloadable local model. Their public
  // readiness check is limited to the non-secret key-presence flag; Rust
  // performs authoritative endpoint/model/key validation before opening audio
  // hardware. Local providers still require the selected model on disk.
  const getSelectedModelStatus = useCallback(async (): Promise<TranscriptionReadiness> => {
    try {
      const { provider, model } = transcriptModelConfig;

      if (isStreamingTranscriptProvider(provider)) {
        return configuredKeyForProvider(transcriptModelConfig, provider)
          ? 'ready'
          : 'missing_api_key';
      }

      let models: LocalTranscriptionModel[];

      if (provider === 'localWhisper') {
        await invoke('whisper_init');
        models = await invoke<WhisperModelInfo[]>('whisper_get_available_models');
      } else {
        await invoke('parakeet_init');
        models = await invoke<ParakeetModelInfo[]>('parakeet_get_available_models');
      }

      const selectedModel = models.find(candidate => candidate.name === model);
      if (selectedModel?.status === 'Available') {
        return 'ready';
      }

      if (
        selectedModel?.status &&
        typeof selectedModel.status === 'object' &&
        'Downloading' in selectedModel.status
      ) {
        return 'downloading';
      }

      return 'missing_local_model';
    } catch (error) {
      console.error(
        `Failed to check ${transcriptModelConfig.provider} model status:`,
        error
      );
      return isStreamingTranscriptProvider(transcriptModelConfig.provider)
        ? 'missing_api_key'
        : 'missing_local_model';
    }
  }, [transcriptModelConfig]);

  const ensureTranscriptionModelReady = useCallback(async (source: RecordingStartSource): Promise<boolean> => {
    const modelStatus = await getSelectedModelStatus();
    if (modelStatus === 'ready') {
      return true;
    }

    if (modelStatus === 'downloading') {
      toast.info('转写模型正在下载', {
        description: '请等待当前选择的本地转写模型下载完成后再开始录音。',
        duration: 5000,
      });
      Analytics.trackButtonClick('start_recording_blocked_downloading', source);
    } else if (modelStatus === 'missing_api_key') {
      const providerName = transcriptModelConfig.provider === 'deepgram'
        ? 'Deepgram'
        : 'OpenAI Realtime';
      toast.error('在线转写尚未配置完成', {
        description: `请先在转写设置中保存 ${providerName} API 密钥。`,
        duration: 5000,
      });
      showModal?.('modelSelector', '请配置在线转写 API 密钥');
      Analytics.trackButtonClick('start_recording_blocked_missing_api_key', source);
    } else {
      toast.error('本地转写模型尚未就绪', {
        description: '请先下载当前选择的本地转写模型。',
        duration: 5000,
      });
      showModal?.('modelSelector', '需要配置转写模型');
      Analytics.trackButtonClick('start_recording_blocked_missing', source);
    }

    setStatus(RecordingStatus.IDLE);
    return false;
  }, [getSelectedModelStatus, setStatus, showModal]);

  // Handle manual recording start (from button click)
  const handleRecordingStart = useCallback(async () => {
    try {
      console.log(
        `handleRecordingStart called - checking ${transcriptModelConfig.provider}/${transcriptModelConfig.model}`
      );

      if (!(await ensureTranscriptionModelReady('home_page'))) {
        return;
      }

      console.log('Transcription model ready - setting up meeting title and state');

      const randomTitle = generateMeetingTitle();
      setMeetingTitle(randomTitle);

      // Set STARTING status before initiating backend recording
      setStatus(RecordingStatus.STARTING, 'Initializing recording...');

      // Start the actual backend recording
      console.log('Starting backend recording');
      liveSummaryService.clearRecordingBindingTicket();
      const startResult = await recordingService.startRecordingWithDevices(
        selectedDevices?.micDevice || null,
        selectedDevices?.systemDevice || null,
        randomTitle
      );
      liveSummaryService.rememberRecordingBindingTicket(
        startResult.summaryBindingTicket ?? null,
      );
      console.log('Backend recording started successfully');

      // Update state after successful backend start
      // Note: RECORDING status will be set by RecordingStateContext event listener
      console.log('Setting isRecordingState to true');
      setIsRecording(true); // This will also update the sidebar via the useEffect
      clearTranscripts(); // Clear previous transcripts when starting new recording
      setIsMeetingActive(true);
      Analytics.trackButtonClick('start_recording', 'home_page');

      // Show recording notification if enabled
      await showRecordingNotification();
    } catch (error) {
      liveSummaryService.clearRecordingBindingTicket();
      console.error('Failed to start recording:', error);
      setStatus(RecordingStatus.ERROR, error instanceof Error ? error.message : 'Failed to start recording');
      setIsRecording(false); // Reset state on error
      Analytics.trackButtonClick('start_recording_error', 'home_page');
      // Re-throw so RecordingControls can handle device-specific errors
      throw error;
    }
  }, [generateMeetingTitle, setMeetingTitle, setIsRecording, clearTranscripts, setIsMeetingActive, ensureTranscriptionModelReady, selectedDevices, setStatus, transcriptModelConfig]);

  // Check for autoStartRecording flag and start recording automatically
  useEffect(() => {
    const checkAutoStartRecording = async () => {
      if (typeof window !== 'undefined') {
        const shouldAutoStart = sessionStorage.getItem('autoStartRecording');
        if (shouldAutoStart === 'true' && !isRecording && !isAutoStarting) {
          console.log('Auto-starting recording from navigation...');
          setIsAutoStarting(true);
          sessionStorage.removeItem('autoStartRecording'); // Clear the flag

          if (!(await ensureTranscriptionModelReady('sidebar_auto'))) {
            setIsAutoStarting(false);
            return;
          }

          // Start the actual backend recording
          try {
            // Generate meeting title
            const generatedMeetingTitle = generateMeetingTitle();

            // Set STARTING status before initiating backend recording
            setStatus(RecordingStatus.STARTING, 'Initializing recording...');

            console.log('Auto-starting backend recording');
            liveSummaryService.clearRecordingBindingTicket();
            const result = await recordingService.startRecordingWithDevices(
              selectedDevices?.micDevice || null,
              selectedDevices?.systemDevice || null,
              generatedMeetingTitle
            );
            liveSummaryService.rememberRecordingBindingTicket(
              result.summaryBindingTicket ?? null,
            );
            console.log('Auto-start backend recording completed');

            // Update UI state after successful backend start
            // Note: RECORDING status will be set by RecordingStateContext event listener
            setMeetingTitle(generatedMeetingTitle);
            setIsRecording(true);
            clearTranscripts();
            setIsMeetingActive(true);
            Analytics.trackButtonClick('start_recording', 'sidebar_auto');

            // Show recording notification if enabled
            await showRecordingNotification();
          } catch (error) {
            liveSummaryService.clearRecordingBindingTicket();
            console.error('Failed to auto-start recording:', error);
            setStatus(RecordingStatus.ERROR, error instanceof Error ? error.message : 'Failed to auto-start recording');
            alert('Failed to start recording. Check console for details.');
            Analytics.trackButtonClick('start_recording_error', 'sidebar_auto');
          } finally {
            setIsAutoStarting(false);
          }
        }
      }
    };

    checkAutoStartRecording();
  }, [
    isRecording,
    isAutoStarting,
    selectedDevices,
    generateMeetingTitle,
    setMeetingTitle,
    setIsRecording,
    clearTranscripts,
    setIsMeetingActive,
    ensureTranscriptionModelReady,
    setStatus,
  ]);

  // Listen for direct recording trigger from sidebar when already on home page
  useEffect(() => {
    const handleDirectStart = async () => {
      if (isRecording || isAutoStarting) {
        console.log('Recording already in progress, ignoring direct start event');
        return;
      }

      console.log('Direct start from sidebar - checking selected transcription model');
      setIsAutoStarting(true);

      if (!(await ensureTranscriptionModelReady('sidebar_direct'))) {
        setIsAutoStarting(false);
        return;
      }

      try {
        // Generate meeting title
        const generatedMeetingTitle = generateMeetingTitle();

        // Set STARTING status before initiating backend recording
        setStatus(RecordingStatus.STARTING, 'Initializing recording...');

        console.log('Starting backend recording');
        liveSummaryService.clearRecordingBindingTicket();
        const result = await recordingService.startRecordingWithDevices(
          selectedDevices?.micDevice || null,
          selectedDevices?.systemDevice || null,
          generatedMeetingTitle
        );
        liveSummaryService.rememberRecordingBindingTicket(
          result.summaryBindingTicket ?? null,
        );
        console.log('Backend recording completed');

        // Update UI state after successful backend start
        // Note: RECORDING status will be set by RecordingStateContext event listener
        setMeetingTitle(generatedMeetingTitle);
        setIsRecording(true);
        clearTranscripts();
        setIsMeetingActive(true);
        Analytics.trackButtonClick('start_recording', 'sidebar_direct');

        // Show recording notification if enabled
        await showRecordingNotification();
      } catch (error) {
        liveSummaryService.clearRecordingBindingTicket();
        console.error('Failed to start recording from sidebar:', error);
        setStatus(RecordingStatus.ERROR, error instanceof Error ? error.message : 'Failed to start recording from sidebar');
        alert('Failed to start recording. Check console for details.');
        Analytics.trackButtonClick('start_recording_error', 'sidebar_direct');
      } finally {
        setIsAutoStarting(false);
      }
    };

    window.addEventListener('start-recording-from-sidebar', handleDirectStart);

    return () => {
      window.removeEventListener('start-recording-from-sidebar', handleDirectStart);
    };
  }, [
    isRecording,
    isAutoStarting,
    selectedDevices,
    generateMeetingTitle,
    setMeetingTitle,
    setIsRecording,
    clearTranscripts,
    setIsMeetingActive,
    ensureTranscriptionModelReady,
    setStatus,
  ]);

  return {
    handleRecordingStart,
    isAutoStarting,
  };
}
