<script lang="ts">
  import { onMount } from 'svelte';
  import { wirelessState } from '../../context/wireless.svelte';
  import WirelessDialog from './WirelessDialog.svelte';
  import PairingRequestToast from './PairingRequestToast.svelte';

  // Single app-wide instance: the top bar button and the "phone wants to pair" notice open the same dialog.
  onMount(() => {
    void wirelessState.init();
    return () => wirelessState.destroy();
  });
</script>

<WirelessDialog open={wirelessState.dialogOpen} onClose={() => wirelessState.close()} />
<PairingRequestToast />
