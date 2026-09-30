<script lang="ts">
  import * as m from '../../paraglide/messages';

  import { wirelessState } from '../../context/wireless.svelte';
  import MaterialIcon from '../MaterialIcon.svelte';

  let request = $derived(wirelessState.request);
</script>

{#if request && !wirelessState.dialogOpen}
  <aside class="pairing-toast" role="status" aria-live="polite">
    <div class="pairing-toast__icon"><MaterialIcon name="wifi_tethering" filled /></div>
    <div class="pairing-toast__text">
      <strong>{m.wireless_request_title({ device: request.label })}</strong>
      <span>{request.endpoint}</span>
    </div>
    <md-text-button onclick={() => wirelessState.dismissRequest()}>{m.common_close()}</md-text-button>
    <md-filled-tonal-button
      onclick={() => wirelessState.open({ tab: 'code', endpoint: request.endpoint })}
    >
      {m.wireless_request_action()}
    </md-filled-tonal-button>
  </aside>
{/if}

<style>
  .pairing-toast {
    position: fixed;
    right: 20px;
    bottom: 20px;
    z-index: 200;
    display: flex;
    align-items: center;
    gap: 12px;
    max-width: min(460px, calc(100vw - 40px));
    padding: 12px 12px 12px 16px;
    color: var(--on-surface);
    background: var(--surface-container-highest);
    border: 1px solid var(--outline-variant);
    border-radius: var(--radius-xl);
    box-shadow: var(--shadow-lg);
    animation: pairing-toast-in 0.28s cubic-bezier(0.2, 0, 0, 1) both;
  }
  .pairing-toast__icon {
    display: grid;
    flex: 0 0 auto;
    place-items: center;
    width: 40px;
    height: 40px;
    color: var(--on-primary-container);
    background: var(--primary-container);
    border-radius: var(--radius-full);
  }
  .pairing-toast__text {
    display: flex;
    flex: 1;
    flex-direction: column;
    min-width: 0;
  }
  .pairing-toast__text strong {
    overflow: hidden;
    font-weight: 500;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pairing-toast__text span {
    color: var(--on-surface-variant);
    font-size: 13px;
    font-variant-numeric: tabular-nums;
  }
  @keyframes pairing-toast-in {
    from { opacity: 0; transform: translateY(12px); }
    to { opacity: 1; transform: translateY(0); }
  }
  @media (prefers-reduced-motion: reduce) {
    .pairing-toast { animation: none; }
  }
</style>
