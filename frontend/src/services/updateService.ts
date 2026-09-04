/**
 * Update Service
 *
 * Handles check-only software update notifications.
 * Installer permissions are intentionally absent from this workspace build.
 */

import { check } from '@tauri-apps/plugin-updater';
import { getVersion } from '@tauri-apps/api/app';
import { invoke } from '@tauri-apps/api/core';

export interface UpdateInfo {
  available: boolean;
  portableMode: boolean;
  currentVersion: string;
  version?: string;
  date?: string;
  body?: string;
}

/**
 * Update Service
 * Singleton service for managing app updates
 */
export class UpdateService {
  private updateCheckInProgress = false;
  private lastCheckTime: number | null = null;
  private portableModePromise: Promise<boolean> | null = null;
  private readonly CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000; // 24 hours

  private async isPortableMode(): Promise<boolean> {
    if (!this.portableModePromise) {
      this.portableModePromise = invoke<boolean>('api_is_portable_mode').catch((error) => {
        console.error('Failed to resolve update storage mode; blocking installer update:', error);
        return true;
      });
    }
    return this.portableModePromise;
  }

  /**
   * Check for available updates
   * @param force Force check even if recently checked
   * @returns Promise with update information
   */
  async checkForUpdates(force = false): Promise<UpdateInfo> {
    // Prevent concurrent update checks
    if (this.updateCheckInProgress) {
      throw new Error('Update check already in progress');
    }

    // Skip if checked recently (unless forced)
    if (!force && this.lastCheckTime) {
      const timeSinceLastCheck = Date.now() - this.lastCheckTime;
      if (timeSinceLastCheck < this.CHECK_INTERVAL_MS) {
        console.log('Skipping update check - checked recently');
        return {
          available: false,
          portableMode: await this.isPortableMode(),
          currentVersion: await getVersion(),
        };
      }
    }

    this.updateCheckInProgress = true;
    this.lastCheckTime = Date.now();

    try {
      const currentVersion = await getVersion();
      const portableMode = await this.isPortableMode();
      const update = await check();

      if (update?.available) {
        return {
          available: true,
          portableMode,
          currentVersion,
          version: update.version,
          date: update.date,
          body: update.body,
        };
      }

      return {
        available: false,
        portableMode,
        currentVersion,
      };
    } catch (error) {
      console.error('Failed to check for updates:', error);
      throw error;
    } finally {
      this.updateCheckInProgress = false;
    }
  }

  /**
   * Get the current app version
   * @returns Promise with version string
   */
  async getCurrentVersion(): Promise<string> {
    return getVersion();
  }

  /**
   * Check if an update check was performed recently
   * @returns true if checked within the interval
   */
  wasCheckedRecently(): boolean {
    if (!this.lastCheckTime) return false;
    const timeSinceLastCheck = Date.now() - this.lastCheckTime;
    return timeSinceLastCheck < this.CHECK_INTERVAL_MS;
  }
}

// Export singleton instance
export const updateService = new UpdateService();
