export interface RecordingPreferencesValue {
  save_folder: string;
  auto_save: boolean;
  file_format: string;
  preferred_mic_device: string | null;
  preferred_system_device: string | null;
  system_audio_backend?: string | null;
}

export interface SelectedAudioDevices {
  micDevice: string | null;
  systemDevice: string | null;
}

export const DISABLED_AUDIO_DEVICE = 'disabled';

export interface SelectableAudioDevice {
  name: string;
  device_type: 'Input' | 'Output';
}

export function audioDeviceOptionValue(device: SelectableAudioDevice): string {
  return `${device.name} (${device.device_type.toLowerCase()})`;
}

export function isSelectedAudioDeviceUnavailable(
  selected: string | null,
  devices: SelectableAudioDevice[],
  type: SelectableAudioDevice['device_type'],
): boolean {
  if (selected === null || selected.toLowerCase() === DISABLED_AUDIO_DEVICE) {
    return false;
  }
  return !devices.some(
    (device) => device.device_type === type && audioDeviceOptionValue(device) === selected,
  );
}

export interface RecordingPreferencesGateway<
  TPreferences extends RecordingPreferencesValue = RecordingPreferencesValue,
> {
  getRecordingPreferences: () => Promise<TPreferences>;
  saveRecordingPreferences: (preferences: TPreferences) => Promise<TPreferences>;
}

export interface RecordingPreferencesResult<
  TPreferences extends RecordingPreferencesValue = RecordingPreferencesValue,
> {
  preferences: TPreferences;
  isLatest: boolean;
  revision: number;
}

export type RecordingPreferencesMutationResult<
  TPreferences extends RecordingPreferencesValue = RecordingPreferencesValue,
> =
  | (RecordingPreferencesResult<TPreferences> & { ok: true })
  | {
      ok: false;
      error: unknown;
      preferences: TPreferences | null;
      isLatest: boolean;
      revision: number;
    };

export class RecordingPreferencesSaveError<
  TPreferences extends RecordingPreferencesValue = RecordingPreferencesValue,
> extends Error {
  readonly recoveredPreferences: TPreferences | null;
  readonly originalError: unknown;

  constructor(originalError: unknown, recoveredPreferences: TPreferences | null) {
    super(originalError instanceof Error ? originalError.message : String(originalError));
    this.name = 'RecordingPreferencesSaveError';
    this.originalError = originalError;
    this.recoveredPreferences = recoveredPreferences;
  }
}

export function selectedDevicesFromPreferences(
  preferences: Pick<
    RecordingPreferencesValue,
    'preferred_mic_device' | 'preferred_system_device'
  >,
): SelectedAudioDevices {
  return {
    micDevice: preferences.preferred_mic_device,
    systemDevice: preferences.preferred_system_device,
  };
}

export function preferencesWithSelectedDevices<
  TPreferences extends RecordingPreferencesValue,
>(preferences: TPreferences, devices: SelectedAudioDevices): TPreferences {
  return {
    ...preferences,
    preferred_mic_device: devices.micDevice,
    preferred_system_device: devices.systemDevice,
  };
}

/**
 * Serializes recording-preference writes and marks stale reads/results.
 *
 * The queue solves two separate races:
 * - a slow startup read must not overwrite a preference saved while it was in flight;
 * - two rapid saves must reach disk in request order, with only the newest result
 *   allowed to update the React context.
 */
export function createRecordingPreferencesCoordinator<
  TPreferences extends RecordingPreferencesValue,
>(gateway: RecordingPreferencesGateway<TPreferences>) {
  let latestRevision = 0;
  let saveTail: Promise<void> = Promise.resolve();
  let lastSuccessfulPreferences: TPreferences | null = null;

  const enqueue = (
    operation: () => Promise<TPreferences>,
  ): Promise<RecordingPreferencesMutationResult<TPreferences>> => {
    const revision = ++latestRevision;
    const pending = saveTail.then(operation);

    // A failed request must not poison the queue for later user changes.
    saveTail = pending.then(
      () => undefined,
      () => undefined,
    );

    return pending.then(
      (preferences) => {
        // Record every successful serialized write, even if a newer request is
        // already waiting. It is the correct rollback target if that newer
        // request later fails.
        lastSuccessfulPreferences = preferences;
        return {
          ok: true as const,
          preferences,
          isLatest: revision === latestRevision,
          revision,
        };
      },
      async (error) => {
        let recoveredPreferences = lastSuccessfulPreferences;

        // When the newest write fails, re-read the canonical backend value.
        // This also covers an initial load that was invalidated by the failed
        // save before it could populate lastSuccessfulPreferences.
        if (revision === latestRevision) {
          try {
            const backendPreferences = await gateway.getRecordingPreferences();
            if (revision === latestRevision) {
              recoveredPreferences = backendPreferences;
              lastSuccessfulPreferences = backendPreferences;
            }
          } catch {
            // Keep the last confirmed successful write. The original save
            // error remains the actionable failure reported to the caller.
          }
        }

        return {
          ok: false as const,
          error,
          preferences: recoveredPreferences,
          isLatest: revision === latestRevision,
          revision,
        };
      },
    );
  };

  return {
    async load(): Promise<RecordingPreferencesResult<TPreferences>> {
      const revision = latestRevision;
      const preferences = await gateway.getRecordingPreferences();
      const isLatest = revision === latestRevision;
      if (isLatest) {
        lastSuccessfulPreferences = preferences;
      }
      return {
        preferences,
        isLatest,
        revision,
      };
    },

    save(preferences: TPreferences): Promise<RecordingPreferencesMutationResult<TPreferences>> {
      return enqueue(() => gateway.saveRecordingPreferences(preferences));
    },

    saveDevices(
      devices: SelectedAudioDevices,
    ): Promise<RecordingPreferencesMutationResult<TPreferences>> {
      return enqueue(async () => {
        // Read inside the queue so unrelated recording preferences from an
        // earlier queued save are preserved by this device-only mutation.
        const current = await gateway.getRecordingPreferences();
        lastSuccessfulPreferences = current;
        return gateway.saveRecordingPreferences(
          preferencesWithSelectedDevices(current, devices),
        );
      });
    },
  };
}
