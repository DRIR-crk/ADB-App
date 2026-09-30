import * as m from '../paraglide/messages';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { devicesState } from './devices.svelte';
import { translateError } from '../pages/workbench/utils';

export type WirelessTab = 'code' | 'qr' | 'manual';

export interface WirelessService {
  kind: 'pairing' | 'connect';
  instance: string;
  label: string;
  host: string;
  port: number;
  endpoint: string;
}

export interface WirelessSnapshot {
  services: WirelessService[];
  builtinActive: boolean;
  builtinError: string | null;
}

export interface PairOutcome {
  message: string;
  guid: string | null;
  serial: string | null;
  connected: boolean;
}

export interface WirelessQr {
  sessionId: string;
  serviceName: string;
  qrData: string;
}

export type PairStatusName = 'waiting' | 'found' | 'pairing' | 'connecting' | 'done' | 'error';

export interface PairStatus {
  sessionId: string;
  status: PairStatusName;
  label: string | null;
  endpoint: string | null;
  serial: string | null;
  connected: boolean;
  error: string | null;
}

const serviceId = (service: WirelessService) => `${service.instance}|${service.port}`;

let discoveryChain: Promise<unknown> = Promise.resolve();

/**
 * `wireless_discovery_acquire` / `_release` are independent async commands and could overtake each
 * other (a quick open → close), which would leave discovery running forever: issue them in order.
 */
export function queueDiscovery(kind: 'acquire' | 'release') {
  discoveryChain = discoveryChain
    .then(() => invoke(`wireless_discovery_${kind}`))
    .catch(() => {});
  return discoveryChain;
}

/** Localized text for the stable error codes returned by the wireless commands. */
export function wirelessErrorText(error: unknown): string {
  const raw = typeof error === 'string'
    ? error
    : (error as { message?: string } | null)?.message ?? String(error);
  switch (raw.trim()) {
    case 'ERROR_WIRELESS_WRONG_CODE': return m.wireless_error_wrong_code();
    case 'ERROR_WIRELESS_UNREACHABLE': return m.wireless_error_unreachable();
    case 'ERROR_WIRELESS_NOT_PAIRED': return m.wireless_error_not_paired();
    case 'ERROR_WIRELESS_INVALID_ENDPOINT': return m.wireless_manual_endpoint_error();
    case 'ERROR_WIRELESS_INVALID_CODE': return m.wireless_error_invalid_code();
    case 'ERROR_WIRELESS_QR_TIMEOUT': return m.wireless_error_qr_timeout();
    default: return translateError(error);
  }
}

class WirelessState {
  /** Pairing and connect services currently advertised on the local network. */
  services = $state.raw<WirelessService[]>([]);
  builtinActive = $state(false);
  builtinError = $state<string | null>(null);

  dialogOpen = $state(false);
  dialogTab = $state<WirelessTab>('code');
  /** Endpoint to preselect in the pairing-code tab (set when opened from the notice). */
  preselect = $state<string | null>(null);

  /** A phone that just asked to pair while the dialog was closed (shown as a notice). */
  request = $state.raw<WirelessService | null>(null);

  #unlisten: UnlistenFn | undefined;
  #initPromise: Promise<void> | undefined;
  /** Bumped by `destroy()`: a listener that resolves after a destroy (or a newer init) is dropped. */
  #generation = 0;
  #known = new Set<string>();

  get pairing() {
    return this.services.filter(service => service.kind === 'pairing');
  }

  /** Starts listening to discovery events. Safe to call more than once. */
  init() {
    this.#initPromise ??= this.#start();
    return this.#initPromise;
  }

  async #start() {
    const generation = this.#generation;
    const unlisten = await listen<WirelessSnapshot>('wireless-services-changed', event => {
      this.#apply(event.payload);
    });
    if (generation !== this.#generation) {
      unlisten();
      return;
    }
    this.#unlisten = unlisten;
    try {
      this.#apply(await invoke<WirelessSnapshot>('wireless_discovery_snapshot'));
    } catch {
      // The next event brings the current state.
    }
  }

  destroy() {
    this.#generation++;
    this.#unlisten?.();
    this.#unlisten = undefined;
    this.#initPromise = undefined;
  }

  #apply(snapshot: WirelessSnapshot) {
    this.services = snapshot.services;
    this.builtinActive = snapshot.builtinActive;
    this.builtinError = snapshot.builtinError;

    const pairing = snapshot.services.filter(service => service.kind === 'pairing');
    const ids = new Set(pairing.map(serviceId));
    const fresh = pairing.filter(service => !this.#known.has(serviceId(service))).at(-1);
    // Forget vanished requests so that a phone opening the dialog again (new port) is announced again.
    this.#known = ids;

    if (fresh && !this.dialogOpen) {
      this.request = fresh;
    } else if (this.request && !ids.has(serviceId(this.request))) {
      this.request = null;
    }
  }

  open(options: { tab?: WirelessTab; endpoint?: string } = {}) {
    this.dialogTab = options.tab ?? 'code';
    this.preselect = options.endpoint ?? null;
    this.request = null;
    this.dialogOpen = true;
  }

  close() {
    this.dialogOpen = false;
    this.preselect = null;
    void devicesState.refreshDevices();
  }

  dismissRequest() {
    this.request = null;
  }
}

export const wirelessState = new WirelessState();
