<script lang="ts">
import * as m from '../../paraglide/messages';

  import { tick, type Snippet } from 'svelte';
  
  import AppModal from './AppModal.svelte';

  let {
    open = false,
    title,
    message,
    confirmText,
    cancelText,
    isDanger = false,
    onConfirm,
    onCancel,
    messageSnippet
  } = $props<{
    open: boolean;
    title: string;
    message?: string;
    confirmText?: string;
    cancelText?: string;
    isDanger?: boolean;
    onConfirm: () => void | Promise<void>;
    onCancel: () => void;
    messageSnippet?: Snippet;
  }>();

  let finalConfirmText = $derived(confirmText || m.common_confirm());
  let finalCancelText = $derived(cancelText || m.common_cancel());
  let errorText = $state('');
  let submitting = $state(false);
  let confirmButton: HTMLElement | undefined = $state();

  $effect(() => {
    if (open) {
      errorText = '';
      submitting = false;
      // Llevar el foco al botón de confirmar: así Enter actúa sobre el botón enfocado y no sobre el elemento de fondo
      tick().then(() => confirmButton?.focus());
    }
  });

  async function handleConfirm() {
    if (submitting) return;
    submitting = true;
    errorText = '';
    try {
      await onConfirm();
    } catch (error) {
      errorText = error instanceof Error ? error.message : String(error);
    } finally {
      submitting = false;
    }
  }

  function handleCancel() {
    if (!submitting) onCancel();
  }

  import { onMount } from 'svelte';
  onMount(() => {
    const handleEnter = (event: KeyboardEvent) => {
      if (!open || event.key !== 'Enter' || event.defaultPrevented) return;
      // Los botones del pie (Cancelar/Confirmar) gestionan su propio Enter; si no, Enter sobre "Cancelar" confirmaría
      const origin = event.target instanceof Element ? event.target : null;
      if (origin?.closest('.app-modal__actions')) return;
      event.preventDefault();
      handleConfirm();
    };
    window.addEventListener('keydown', handleEnter);
    return () => window.removeEventListener('keydown', handleEnter);
  });
</script>

<AppModal {open} {title} onClose={handleCancel} width="compact" cancelText={finalCancelText} cancelDisabled={submitting}>
  <p style="margin: 0; color: var(--on-surface-variant); line-height: 1.5;white-space: pre-wrap;">
    {#if messageSnippet}
      {@render messageSnippet()}
    {:else}
      {message}
    {/if}
  </p>
  {#if errorText}
    <p class="confirm-dialog__error" role="alert">{errorText}</p>
  {/if}
  
  {#snippet actions()}
    <md-filled-button
      bind:this={confirmButton}
      disabled={submitting ? true : undefined}
      onclick={handleConfirm}
      class={isDanger ? 'md-btn-danger' : ''}
    >
      {finalConfirmText}
    </md-filled-button>
  {/snippet}
</AppModal>

<style>
.confirm-dialog__error{margin:12px 0 0;color:var(--error);font-size:12px;line-height:1.4}
</style>
