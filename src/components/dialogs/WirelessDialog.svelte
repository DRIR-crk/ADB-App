<script lang="ts">
  import * as m from '../../paraglide/messages';

  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, tick, untrack } from 'svelte';
  import { devicesState } from '../../context/devices.svelte';
  import {
    wirelessState,
    wirelessErrorText,
    queueDiscovery,
    type PairOutcome,
    type PairStatus,
    type WirelessQr,
    type WirelessTab,
  } from '../../context/wireless.svelte';

  import MaterialIcon from '../MaterialIcon.svelte';
  import AppModal from './AppModal.svelte';
  import { materialTextFieldValue } from '../../actions/materialTextFieldValue';

  let { open = false, onClose } = $props<{ open: boolean; onClose: () => void }>();

  type Busy = 'idle' | 'pairing' | 'connecting';

  const SEARCH_HINT_DELAY_MS = 12_000;
  const SUCCESS_CLOSE_DELAY_MS = 800;
  const MAX_TRACKED_SESSIONS = 8;

  let tab = $state<WirelessTab>('code');
  let busy = $state<Busy>('idle');
  let errorText = $state('');
  let successText = $state('');
  let runToken = 0;

  // Pairing code tab
  let chosenEndpoint = $state<string | null>(null);
  let code = $state('');
  let codeField = $state<HTMLElement | undefined>();
  let searchHintVisible = $state(false);

  // Manual tab
  let endpoint = $state('');
  let endpointError = $state(false);
  let manualCode = $state('');

  // QR tab
  let qrSession = $state<WirelessQr | null>(null);
  let qrImage = $state('');
  let qrError = $state('');
  let qrToken = 0;
  let handledQrResult = '';

  // Progress of pairing attempts (QR sessions and pairing-code attempts), keyed by session id.
  let pairStatuses = $state.raw<Record<string, PairStatus>>({});
  let flowSession = $state<string | null>(null);

  let pairingDevices = $derived(wirelessState.services.filter(service => service.kind === 'pairing'));
  // A single detected phone is selected for the user, so only the code has to be typed.
  let selectedDevice = $derived(
    pairingDevices.find(device => device.endpoint === chosenEndpoint)
      ?? (pairingDevices.length === 1 ? pairingDevices[0] : null),
  );
  let selectedEndpoint = $derived(selectedDevice?.endpoint ?? null);
  let codeValid = $derived(/^\d{6}$/.test(code));
  let flowStatus = $derived(flowSession ? pairStatuses[flowSession]?.status : undefined);
  let loadingText = $derived(
    busy === 'connecting' || flowStatus === 'connecting'
      ? m.wireless_connect_pending()
      : m.wireless_pair_pending(),
  );

  let qrStatus = $derived(qrSession ? pairStatuses[qrSession.sessionId] : undefined);
  let qrBusy = $derived(
    qrStatus?.status === 'found' || qrStatus?.status === 'pairing' || qrStatus?.status === 'connecting',
  );
  let qrStatusText = $derived.by(() => {
    switch (qrStatus?.status) {
      case 'found': return m.wireless_qr_found({ device: qrStatus.label ?? '' });
      case 'pairing': return m.wireless_pair_pending();
      case 'connecting': return m.wireless_connect_pending();
      case 'done': return qrStatus.connected ? m.wireless_success_connected() : m.wireless_success_paired_only();
      case 'error': return wirelessErrorText(qrStatus.error);
      default: return m.wireless_qr_waiting();
    }
  });

  onMount(() => {
    let disposed = false;
    let stopListening: (() => void) | undefined;
    listen<PairStatus>('wireless-pair-status', event => {
      const next = { ...pairStatuses, [event.payload.sessionId]: event.payload };
      const ids = Object.keys(next);
      for (const id of ids.slice(0, Math.max(0, ids.length - MAX_TRACKED_SESSIONS))) delete next[id];
      pairStatuses = next;
    }).then(unlisten => {
      if (disposed) unlisten();
      else stopListening = unlisten;
    });
    return () => {
      disposed = true;
      stopListening?.();
    };
  });

  // Discovery only runs while the dialog is open; the backend keeps it alive for as long as needed.
  $effect(() => {
    if (!open) return;
    untrack(resetForOpen);
    void queueDiscovery('acquire');
    return () => {
      runToken++;
      untrack(stopQr);
      void queueDiscovery('release');
    };
  });

  $effect(() => {
    if (!open || tab !== 'qr') return;
    untrack(startQr);
    // Leaving the tab must not abort a pairing that is already in progress.
    return () => untrack(() => { if (!qrBusy) stopQr(); });
  });

  // Offer help if nothing shows up for a while.
  $effect(() => {
    searchHintVisible = false;
    if (!open || tab !== 'code' || pairingDevices.length > 0) return;
    const timer = setTimeout(() => { searchHintVisible = true; }, SEARCH_HINT_DELAY_MS);
    return () => clearTimeout(timer);
  });

  // Once a phone is selected (and again after a failed attempt), the only thing left is the code.
  $effect(() => {
    if (open && tab === 'code' && busy === 'idle' && selectedEndpoint) {
      void tick().then(() => codeField?.focus());
    }
  });

  // React to the result of a QR session once.
  $effect(() => {
    const status = qrStatus;
    if (!status || status.status !== 'done') return;
    const key = `${status.sessionId}:done`;
    if (key === handledQrResult) return;
    handledQrResult = key;
    untrack(() => void finishPairing(status.serial, status.connected, hostOf(status.endpoint ?? '')));
  });

  function resetForOpen() {
    tab = wirelessState.dialogTab;
    chosenEndpoint = wirelessState.preselect;
    busy = 'idle';
    errorText = '';
    successText = '';
    code = '';
    endpoint = '';
    manualCode = '';
    endpointError = false;
    flowSession = null;
    qrError = '';
    handledQrResult = '';
  }

  function changeTab(next: WirelessTab) {
    if (tab === next) return;
    tab = next;
    errorText = '';
    successText = '';
    endpointError = false;
  }

  function closeDialog() {
    if (open) onClose();
  }

  async function renderQr(data: string) {
    const { default: QRCode } = await import('qrcode');
    // Vector output stays crisp at any size / display scaling, which makes scanning reliable.
    const svg = await QRCode.toString(data, {
      type: 'svg',
      margin: 2,
      errorCorrectionLevel: 'M',
      color: { dark: '#000000', light: '#ffffff' },
    });
    return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
  }

  async function startQr() {
    // Coming back to the tab while a scan is being paired must not throw that work away.
    if (qrSession && qrBusy) return;
    stopQr();
    qrError = '';
    const token = ++qrToken;
    let session: WirelessQr | null = null;
    try {
      session = await invoke<WirelessQr>('start_wireless_qr');
      const image = await renderQr(session.qrData);
      if (token !== qrToken) throw new Error('superseded');
      qrSession = session;
      qrImage = image;
    } catch (error) {
      if (session) void invoke('cancel_wireless_qr', { sessionId: session.sessionId }).catch(() => {});
      if (token === qrToken) qrError = wirelessErrorText(error);
    }
  }

  function stopQr() {
    qrToken++;
    const session = qrSession;
    qrSession = null;
    qrImage = '';
    if (session) void invoke('cancel_wireless_qr', { sessionId: session.sessionId }).catch(() => {});
  }

  /** Selects the freshly paired device, or asks for the connect port when it could not be found. */
  async function finishPairing(serial: string | null, connected: boolean, pairedHost = '') {
    await devicesState.refreshDevices(serial ?? undefined);
    busy = 'idle';
    if (connected) {
      successText = m.wireless_success_connected();
      // A reopened dialog (new run token) must not be closed by this timer.
      const token = runToken;
      setTimeout(() => { if (token === runToken) closeDialog(); }, SUCCESS_CLOSE_DELAY_MS);
      return;
    }
    // Paired, but the phone's connection port is unknown: continue manually with the IP prefilled.
    successText = m.wireless_success_paired_only();
    if (pairedHost) endpoint = `${pairedHost}:`;
    tab = 'manual';
  }

  function hostOf(target: string) {
    const separator = target.lastIndexOf(':');
    return separator > 0 ? target.slice(0, separator) : target;
  }

  async function runPair(target: string, pairingCode: string) {
    const token = ++runToken;
    const session = crypto.randomUUID();
    flowSession = session;
    busy = 'pairing';
    errorText = '';
    successText = '';
    try {
      const outcome = await invoke<PairOutcome>('pair_wireless_device', {
        endpoint: target,
        code: pairingCode,
        sessionId: session,
      });
      if (token !== runToken) return;
      await finishPairing(outcome.serial, outcome.connected, hostOf(target));
    } catch (error) {
      if (token !== runToken) return;
      errorText = wirelessErrorText(error);
      busy = 'idle';
      code = '';
      manualCode = '';
    }
  }

  async function runConnect(target: string) {
    const token = ++runToken;
    busy = 'connecting';
    flowSession = null;
    errorText = '';
    successText = '';
    try {
      await invoke<string>('connect_wireless_device', { endpoint: target });
      if (token !== runToken) return;
      await finishPairing(target, true);
    } catch (error) {
      if (token !== runToken) return;
      errorText = wirelessErrorText(error);
      busy = 'idle';
    }
  }

  function pairSelected() {
    if (!selectedDevice || !codeValid || busy !== 'idle') return;
    void runPair(selectedDevice.endpoint, code);
  }

  function submitManual() {
    if (busy !== 'idle') return;
    errorText = '';
    const target = endpoint.trim();
    if (!/^(\[[0-9a-fA-F:.%]+\]|[A-Za-z0-9._-]+):\d{1,5}$/.test(target)) {
      endpointError = true;
      return;
    }
    endpointError = false;
    if (manualCode.trim()) void runPair(target, manualCode);
    else void runConnect(target);
  }

  function selectDevice(target: string) {
    chosenEndpoint = target;
    errorText = '';
  }

  function onCodeInput(event: Event) {
    const field = event.target as HTMLInputElement;
    const digits = field.value.replace(/\D/g, '').slice(0, 6);
    if (field.value !== digits) field.value = digits;
    code = digits;
    errorText = '';
  }

  function onCodeKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault();
      pairSelected();
    }
  }

  function onManualKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault();
      submitManual();
    }
  }
</script>

<AppModal {open} onClose={closeDialog} title={m.wireless_title()} cancelDisabled={qrBusy}>
  {#snippet actions()}
    {#if tab === 'code'}
      <md-filled-button
        disabled={!selectedDevice || !codeValid || busy !== 'idle' ? true : undefined}
        onclick={pairSelected}
      >
        {m.wireless_action_pair()}
      </md-filled-button>
    {:else if tab === 'manual'}
      <md-filled-button
        disabled={!endpoint.trim() || busy !== 'idle' ? true : undefined}
        onclick={submitManual}
      >
        {manualCode.trim() ? m.wireless_action_pair() : m.wireless_action_connect()}
      </md-filled-button>
    {/if}
  {/snippet}

  <md-tabs class="wireless-tabs">
    <md-primary-tab active={tab === 'code' ? true : undefined} onclick={() => changeTab('code')}>
      {m.wireless_tab_code()}
    </md-primary-tab>
    <md-primary-tab active={tab === 'qr' ? true : undefined} onclick={() => changeTab('qr')}>
      {m.wireless_tab_qr()}
    </md-primary-tab>
    <md-primary-tab active={tab === 'manual' ? true : undefined} onclick={() => changeTab('manual')}>
      {m.wireless_tab_manual()}
    </md-primary-tab>
  </md-tabs>

  {#if tab === 'code'}
    <section class="wireless-pane">
      {#if busy !== 'idle'}
        <div class="wireless-loading">
          <md-circular-progress indeterminate></md-circular-progress>
          <p>{loadingText}</p>
        </div>
      {:else}
        <p class="wireless-hint">{m.wireless_code_desc()}</p>

        {#if pairingDevices.length === 0}
          <div class="wireless-searching">
            <md-circular-progress indeterminate></md-circular-progress>
            <span>{m.wireless_code_searching()}</span>
          </div>
          {#if searchHintVisible}
            <p class="wireless-note">
              {m.wireless_code_not_found()}
              <md-text-button onclick={() => changeTab('manual')}>{m.wireless_code_manual_link()}</md-text-button>
            </p>
          {/if}
        {:else}
          <div class="wireless-devices-title">{m.wireless_code_detected()}</div>
          <ul class="wireless-devices">
            {#each pairingDevices as device (device.instance + device.port)}
              {@const selected = selectedDevice?.endpoint === device.endpoint}
              <li>
                <button
                  type="button"
                  class="wireless-device"
                  class:selected
                  aria-pressed={selected}
                  onclick={() => selectDevice(device.endpoint)}
                >
                  <MaterialIcon name="smartphone" />
                  <span class="wireless-device__text">
                    <strong>{device.label}</strong>
                    <small>{device.endpoint}</small>
                  </span>
                  {#if selected}<MaterialIcon name="check_circle" filled />{/if}
                </button>
              </li>
            {/each}
          </ul>

          {#if selectedDevice}
            <md-outlined-text-field
              class="wireless-code"
              bind:this={codeField}
              label={m.wireless_code_field()}
              supporting-text={m.wireless_code_helper()}
              inputmode="numeric"
              maxlength="6"
              autocomplete="off"
              use:materialTextFieldValue={code}
              oninput={onCodeInput}
              onkeydown={onCodeKeydown}
              error={errorText ? true : undefined}
              error-text={errorText}
            ></md-outlined-text-field>
          {/if}
        {/if}

        {#if errorText && !selectedDevice}
          <div class="wireless-error"><MaterialIcon name="error" /><span>{errorText}</span></div>
        {/if}
        {#if successText}
          <div class="wireless-success"><MaterialIcon name="check_circle" filled /><span>{successText}</span></div>
        {/if}
      {/if}
    </section>
  {:else if tab === 'qr'}
    <section class="wireless-qr">
      <div class="wireless-qr__frame" class:dimmed={qrBusy || qrStatus?.status === 'done' || qrStatus?.status === 'error'}>
        {#if qrImage}
          <img src={qrImage} alt={m.wireless_qr_alt()} draggable="false" />
        {:else if qrError}
          <MaterialIcon name="qr_code_2" />
        {:else}
          <md-circular-progress indeterminate></md-circular-progress>
        {/if}
        {#if qrBusy}
          <div class="wireless-qr__overlay"><md-circular-progress indeterminate></md-circular-progress></div>
        {:else if qrStatus?.status === 'done'}
          <div class="wireless-qr__overlay"><MaterialIcon name="check_circle" filled size={56} /></div>
        {/if}
      </div>
      <div class="wireless-qr__info">
        <p class="wireless-hint">{m.wireless_qr_desc()}</p>
        {#if qrError}
          <div class="wireless-error"><MaterialIcon name="error" /><span>{qrError}</span></div>
        {:else if qrStatus?.status === 'error'}
          <div class="wireless-error"><MaterialIcon name="error" /><span>{qrStatusText}</span></div>
        {:else if qrStatus?.status === 'done'}
          <div class="wireless-success"><MaterialIcon name="check_circle" filled /><span>{qrStatusText}</span></div>
        {:else}
          <div class="wireless-status"><span>{qrStatusText}</span></div>
        {/if}
        <div>
          {#if qrError || qrStatus?.status === 'error'}
            <md-filled-tonal-button onclick={() => void startQr()}>
              <MaterialIcon slot="icon" name="refresh" />
              {m.wireless_qr_regenerate()}
            </md-filled-tonal-button>
          {:else}
            <md-text-button
              disabled={qrBusy ? true : undefined}
              onclick={() => void startQr()}
            >
              <MaterialIcon slot="icon" name="refresh" />
              {m.wireless_qr_regenerate()}
            </md-text-button>
          {/if}
        </div>
      </div>
    </section>
  {:else}
    <section class="wireless-pane">
      {#if busy !== 'idle'}
        <div class="wireless-loading">
          <md-circular-progress indeterminate></md-circular-progress>
          <p>{loadingText}</p>
        </div>
      {:else}
        <p class="wireless-hint">{m.wireless_manual_desc()}</p>
        <md-outlined-text-field
          label={m.wireless_manual_endpoint()}
          use:materialTextFieldValue={endpoint}
          oninput={(event: any) => { endpoint = event.target.value; endpointError = false; errorText = ''; }}
          onkeydown={onManualKeydown}
          error={endpointError || !!errorText ? true : undefined}
          error-text={endpointError ? m.wireless_manual_endpoint_error() : errorText}
        ></md-outlined-text-field>
        <md-outlined-text-field
          label={m.wireless_manual_code()}
          autocomplete="off"
          use:materialTextFieldValue={manualCode}
          oninput={(event: any) => { manualCode = event.target.value; }}
          onkeydown={onManualKeydown}
        ></md-outlined-text-field>
        {#if successText}
          <div class="wireless-success"><MaterialIcon name="check_circle" filled /><span>{successText}</span></div>
        {/if}
      {/if}
    </section>
  {/if}
</AppModal>

<style>
  .wireless-tabs, md-primary-tab {
    --md-primary-tab-container-color: transparent;
    --md-sys-color-surface: transparent;
    background-color: transparent;
  }
  .wireless-pane {
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding-top: 18px;
  }
  .wireless-hint,
  .wireless-note {
    margin: 0;
    color: var(--on-surface-variant);
    line-height: 1.45;
  }
  .wireless-note {
    font-size: 14px;
  }
  .wireless-searching {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 18px 16px;
    color: var(--on-surface-variant);
    background: var(--surface-container);
    border-radius: var(--radius-lg);
  }
  .wireless-searching md-circular-progress,
  .wireless-qr__overlay md-circular-progress {
    --md-circular-progress-size: 32px;
  }
  .wireless-devices-title {
    color: var(--on-surface-variant);
    font-size: 13px;
    font-weight: 500;
  }
  .wireless-devices {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .wireless-device {
    display: flex;
    align-items: center;
    gap: 14px;
    width: 100%;
    padding: 12px 14px;
    color: var(--on-surface);
    text-align: left;
    cursor: pointer;
    background: var(--surface-container);
    border: 1px solid var(--outline-variant);
    border-radius: var(--radius-lg);
    transition: background var(--transition-fast), border-color var(--transition-fast);
  }
  .wireless-device:hover {
    background: var(--surface-container-highest);
  }
  .wireless-device.selected {
    color: var(--on-primary-container);
    background: var(--primary-container);
    border-color: var(--primary);
  }
  .wireless-device__text {
    display: flex;
    flex: 1;
    flex-direction: column;
    min-width: 0;
  }
  .wireless-device__text strong {
    overflow: hidden;
    font-weight: 500;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .wireless-device__text small {
    color: var(--on-surface-variant);
    font-variant-numeric: tabular-nums;
  }
  .wireless-device.selected .wireless-device__text small {
    color: inherit;
    opacity: 0.8;
  }
  .wireless-code {
    --md-outlined-text-field-input-text-size: 26px;
    --md-outlined-text-field-input-text-tracking: 0.28em;
    width: 100%;
  }
  .wireless-loading {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 16px;
    padding: 32px 0;
    color: var(--on-surface);
    text-align: center;
  }
  .wireless-error,
  .wireless-success,
  .wireless-status {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 14px;
  }
  .wireless-error,
  .wireless-error :global(.material-symbols-rounded) {
    color: var(--color-red, #d32f2f);
  }
  .wireless-success,
  .wireless-success :global(.material-symbols-rounded) {
    color: var(--color-green, #4caf50);
  }
  .wireless-status {
    color: var(--on-surface-variant);
  }
  .wireless-qr {
    display: grid;
    grid-template-columns: 252px 1fr;
    align-items: center;
    gap: 24px;
    padding-top: 18px;
  }
  .wireless-qr__frame {
    position: relative;
    display: grid;
    place-items: center;
    width: 252px;
    height: 252px;
    overflow: hidden;
    /* White on purpose, in every theme: scanners need dark modules on a light quiet zone. */
    background: #fff;
    border-radius: var(--radius-lg);
  }
  .wireless-qr__frame img {
    display: block;
    width: 100%;
    height: 100%;
    transition: opacity var(--transition-base);
    user-select: none;
  }
  .wireless-qr__frame.dimmed img {
    opacity: 0.12;
  }
  .wireless-qr__frame :global(.material-symbols-rounded) {
    color: #555;
    font-size: 64px;
  }
  .wireless-qr__overlay {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
  }
  .wireless-qr__overlay :global(.material-symbols-rounded) {
    color: var(--color-green, #4caf50);
  }
  .wireless-qr__info {
    display: flex;
    flex-direction: column;
    gap: 12px;
    min-width: 0;
  }
  @media (max-width: 620px) {
    .wireless-qr {
      grid-template-columns: 1fr;
      justify-items: center;
    }
  }
</style>
