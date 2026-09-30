import { invoke } from '@tauri-apps/api/core';
import { emit, listen } from '@tauri-apps/api/event';
import { devicesState } from './devices.svelte';

// Exact error string returned by `sideload_device` (Rust) when the user cancels.
const CANCELLED_BY_USER = 'Cancelled by user';

class SideloadState {
  busy = $state(false);
  progress = $state(0);
  serial = $state<string | null>(null);
  error = $state<string | null>(null);

  async start(serial: string, filePath: string) {
    if (this.busy) return;
    this.busy = true;
    this.progress = 0;
    this.serial = serial;
    this.error = null;

    let unlisten: (() => void) | undefined;
    try {
      unlisten = await listen<number>('sideload-progress', event => {
        this.progress = event.payload;
      });
      await invoke('sideload_device', { serial, filePath });
      setTimeout(() => devicesState.refreshDevices(), 2000);
    } catch (error) {
      const message = String(error);
      if (message !== CANCELLED_BY_USER) this.error = message;
    } finally {
      this.busy = false;
      unlisten?.();
    }
  }

  cancel() {
    if (!this.busy) return;
    void emit('cancel-sideload');
  }
}

export const sideloadState = new SideloadState();
