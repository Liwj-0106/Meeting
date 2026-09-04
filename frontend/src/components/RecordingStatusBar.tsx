'use client';

import { motion, useReducedMotion } from 'framer-motion';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import {
  isDegradedAudioSourceEvent,
  listAudioSourceHealth,
  presentAudioSourceHealth,
  type AudioHealthTone,
} from '@/lib/audio-source-health';
import { presentAsrHealth } from '@/lib/asr-health';
import { useEffect, useMemo, useState } from 'react';

interface RecordingStatusBarProps {
  isPaused?: boolean;
}

const TONE_CLASSES: Record<AudioHealthTone, string> = {
  healthy: 'border-emerald-200 bg-emerald-50 text-emerald-800 dark:border-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-200',
  progress: 'border-blue-200 bg-blue-50 text-blue-800 dark:border-blue-800 dark:bg-blue-950/40 dark:text-blue-200',
  warning: 'border-amber-200 bg-amber-50 text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200',
  fatal: 'border-red-200 bg-red-50 text-red-800 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200',
  neutral: 'border-gray-200 bg-white text-gray-700 dark:border-gray-700 dark:bg-gray-900 dark:text-gray-200',
};

const DOT_CLASSES: Record<AudioHealthTone, string> = {
  healthy: 'bg-emerald-500',
  progress: 'bg-blue-500',
  warning: 'bg-amber-500',
  fatal: 'bg-red-500',
  neutral: 'bg-gray-400',
};

function formatDuration(seconds: number): string {
  const mins = Math.floor(seconds / 60);
  const secs = seconds % 60;
  return `${mins.toString().padStart(2, '0')}:${secs.toString().padStart(2, '0')}`;
}

export const RecordingStatusBar: React.FC<RecordingStatusBarProps> = ({ isPaused = false }) => {
  // Get recording duration from backend-synced context (in seconds)
  // Backend polls every 500ms, providing smooth updates
  const { activeDuration, isRecording, sourceHealth, asrHealth } = useRecordingState();
  const reduceMotion = useReducedMotion();

  // Display state synced from backend
  const [displaySeconds, setDisplaySeconds] = useState(0);

  // Sync with backend duration when it changes (handles refresh/navigation)
  useEffect(() => {
    if (activeDuration !== null) {
      // Round to nearest second to avoid decimal issues
      setDisplaySeconds(Math.floor(activeDuration));
    }
  }, [activeDuration]);

  const visibleSourceHealth = useMemo(() => {
    if (!isRecording) {
      return [];
    }

    const sources = listAudioSourceHealth(sourceHealth);
    const hasDirectCaptureSource = sources.some(
      event => event.track === 'microphone' || event.track === 'system',
    );

    // A healthy mixed stream duplicates the two capture badges. Keep it only
    // when it is the sole source or has a condition the user should see.
    return sources.filter(event => (
      event.track !== 'mixed' ||
      !hasDirectCaptureSource ||
      isDegradedAudioSourceEvent(event)
    ));
  }, [isRecording, sourceHealth]);

  const hasFatalHealth = visibleSourceHealth.some(
    event => presentAudioSourceHealth(event).tone === 'fatal',
  );
  const asrPresentation = useMemo(
    () => asrHealth ? presentAsrHealth(asrHealth) : null,
    [asrHealth],
  );

  return (
    <motion.div
      initial={reduceMotion ? false : { opacity: 0, y: -10 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, y: -10 }}
      transition={{ duration: reduceMotion ? 0 : 0.2 }}
      className="mb-2 flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg bg-gray-50 px-3 py-2 dark:bg-gray-900"
    >
      <div className="flex shrink-0 items-center gap-2">
        <div
          aria-hidden="true"
          className={`h-2 w-2 rounded-full ${isPaused ? 'bg-orange-500' : 'bg-red-500 motion-safe:animate-pulse'}`}
        />
        <span className={`text-sm tabular-nums ${isPaused ? 'text-orange-700 dark:text-orange-300' : 'text-gray-700 dark:text-gray-200'}`}>
          {isPaused ? '已暂停' : '正在录音'} • {formatDuration(displaySeconds)}
        </span>
      </div>

      {visibleSourceHealth.length > 0 && (
        <div
          className="flex min-w-0 flex-wrap items-center gap-1.5"
          role={hasFatalHealth ? 'alert' : 'status'}
          aria-live={hasFatalHealth ? 'assertive' : 'polite'}
          aria-atomic="false"
          aria-label="录音音源状态"
        >
          {visibleSourceHealth.map(event => {
            const presentation = presentAudioSourceHealth(event);
            return (
              <span
                key={event.track}
                className={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs font-medium leading-5 ${TONE_CLASSES[presentation.tone]}`}
                title={presentation.detail}
              >
                <span
                  aria-hidden="true"
                  className={`h-1.5 w-1.5 shrink-0 rounded-full ${DOT_CLASSES[presentation.tone]}`}
                />
                {presentation.label}
              </span>
            );
          })}
        </div>
      )}

      {isRecording && asrHealth && asrHealth.state !== 'stopped' && asrPresentation && (
        <span
          className={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs font-medium leading-5 ${TONE_CLASSES[asrPresentation.tone]}`}
          role={asrPresentation.tone === 'fatal' ? 'alert' : 'status'}
          aria-live={asrPresentation.tone === 'fatal' ? 'assertive' : 'polite'}
          title={asrPresentation.detail}
        >
          <span
            aria-hidden="true"
            className={`h-1.5 w-1.5 shrink-0 rounded-full ${DOT_CLASSES[asrPresentation.tone]}`}
          />
          {asrPresentation.label}
        </span>
      )}
    </motion.div>
  );
};
