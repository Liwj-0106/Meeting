import { invoke } from '@tauri-apps/api/core';
import { Store, type StoreOptions } from '@tauri-apps/plugin-store';

let dataDirectoryPromise: Promise<string> | null = null;

async function getDataDirectory(): Promise<string> {
  if (!dataDirectoryPromise) {
    dataDirectoryPromise = invoke<string>('get_database_directory').catch((error) => {
      dataDirectoryPromise = null;
      throw error;
    });
  }

  return dataDirectoryPromise;
}

export async function loadAppStore(
  fileName: string,
  options?: StoreOptions,
): Promise<Store> {
  const dataDirectory = (await getDataDirectory()).replace(/[\\/]$/, '');
  return Store.load(`${dataDirectory}/${fileName}`, options);
}
