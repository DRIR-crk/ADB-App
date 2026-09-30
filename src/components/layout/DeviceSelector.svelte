<script lang="ts">
import * as m from '../../paraglide/messages';

  import { onMount } from 'svelte';
  
  import { isWirelessSerial, type Device, type DeviceDetails } from '../../context/devices.svelte';
  import MaterialIcon from '../MaterialIcon.svelte';
  import { getMarketingName } from '../../pages/workbench/utils';

  function formatLabel(marketingName: string, model: string) {
    return marketingName ? `${marketingName} (${model})` : model;
  }

  function getShortDeviceName(device: Device | null) {
    if (!device) return null;
    if (device.model) {
      let marketingName = getMarketingName(device.model);
      if (!marketingName) {
         const cleanModel = device.model.replace(/_/g, '-').toUpperCase();
         marketingName = getMarketingName(cleanModel);
      }
      return marketingName || device.model;
    }
    return device.serial;
  }

  function getDeviceName(device: Device | null) {
    if (!device) return null;
    if (device.model) {
      let marketingName = getMarketingName(device.model);
      if (!marketingName) {
         const cleanModel = device.model.replace(/_/g, '-').toUpperCase();
         marketingName = getMarketingName(cleanModel);
      }
      return formatLabel(marketingName, device.model);
    }
    return device.serial;
  }

  let {
    devices,
    selectedDevice,
    deviceDetails = null,
    loading,
    loadingLabel,
    emptyLabel,
    onSelect,
    onDisconnect
  } = $props<{
    devices: Device[];
    selectedDevice: Device | null;
    deviceDetails?: DeviceDetails | null;
    loading: boolean;
    loadingLabel: string;
    emptyLabel: string;
    onSelect: (serial: string) => void;
    onDisconnect?: (serial: string) => void;
  }>();

  let anchorElement: HTMLButtonElement | undefined = $state();
  let menuElement: any | undefined = $state();
  let open = $state(false);

  let disabled = $derived(loading || devices.length === 0);
  let label = $derived(
    (deviceDetails 
      ? (getMarketingName(deviceDetails.model, deviceDetails.brand) || deviceDetails.model)
      : getShortDeviceName(selectedDevice))
    || (loading ? loadingLabel : emptyLabel)
  );
  let connectionIcon = $derived(selectedDevice && isWirelessSerial(selectedDevice.serial) ? 'wifi' : 'smartphone');

  function stateLabel(state: string) {
    switch (state) {
      case 'device': return m.state_connected();
      case 'offline': return m.state_offline();
      case 'unauthorized': return m.state_unauthorized();
      case 'connecting': return m.state_connecting();
      default: return state;
    }
  }

  onMount(() => {
    if (!menuElement || !anchorElement) return;
    
    menuElement.anchorElement = anchorElement;
    
    const opening = () => open = true;
    const closed = () => open = false;
    const selected = (event: Event) => {
      const detail = (event as CustomEvent<{ initiator?: HTMLElement }>).detail;
      const serial = detail?.initiator?.dataset.deviceSerial;
      if (serial) onSelect(serial);
    };
    
    menuElement.addEventListener('opening', opening);
    menuElement.addEventListener('closed', closed);
    menuElement.addEventListener('close-menu', selected);
    
    return () => {
      menuElement.removeEventListener('opening', opening);
      menuElement.removeEventListener('closed', closed);
      menuElement.removeEventListener('close-menu', selected);
    };
  });

  async function toggleMenu() {
    if (disabled) return;
    if (!menuElement || !anchorElement) return;
    
    menuElement.anchorElement = anchorElement;
    const width = `${anchorElement.getBoundingClientRect().width}px`;
    menuElement.style.setProperty('--md-menu-container-width', width);
    menuElement.style.setProperty('max-width', width);
    menuElement.style.setProperty('min-width', width);
    menuElement.style.width = width;
    
    if (menuElement.open) {
      menuElement.close();
    } else {
      menuElement.show();
    }
  }

  // Native listeners on purpose: Svelte delegates on* handlers to the root, which runs after
  // md-menu-item's internal click handler, so a delegated stopPropagation would still let the
  // click select (and the menu close on) the device being disconnected.
  function disconnectButton(node: HTMLElement, serial: string) {
    let current = serial;
    const onClick = (event: Event) => {
      event.stopPropagation();
      event.preventDefault();
      if (onDisconnect) onDisconnect(current);
    };
    const onKeydown = (event: KeyboardEvent) => {
      // Only Enter/Space activate the button (native activation dispatches the click above);
      // keep them from reaching the menu item, let arrows/Tab through for menu navigation.
      if (event.key === 'Enter' || event.key === ' ') event.stopPropagation();
    };
    node.addEventListener('click', onClick);
    node.addEventListener('keydown', onKeydown);
    return {
      update(next: string) { current = next; },
      destroy() {
        node.removeEventListener('click', onClick);
        node.removeEventListener('keydown', onKeydown);
      },
    };
  }
</script>

<div class="topbar-device-picker">
  <button
    bind:this={anchorElement}
    class="topbar-device-picker__field {open ? 'open' : ''}"
    type="button"
    aria-label={m.device_selector_label()}
    aria-haspopup="menu"
    aria-expanded={open}
    {disabled}
    onclick={toggleMenu}
    ondblclick={e => e.stopPropagation()}
  >
    <MaterialIcon name={selectedDevice ? connectionIcon : 'devices'} />
    <span class="topbar-device-picker__label">{label}</span>
    {#if selectedDevice}
      <span class="topbar-device-picker__status {selectedDevice.state === 'device' ? 'connected' : ''}" aria-label={stateLabel(selectedDevice.state)}></span>
    {/if}
    <MaterialIcon name="arrow_drop_down" class="topbar-device-picker__arrow" />
    <md-ripple></md-ripple>
  </button>
  
  <md-menu
    bind:this={menuElement}
    class="topbar-device-picker__menu"
    positioning="popover"
    anchorCorner="end-start"
    menuCorner="start-start"
  >
    {#each devices as device (device.serial)}
      {@const isWireless = isWirelessSerial(device.serial)}
      {@const deviceName = getDeviceName(device)}
      <md-menu-item
        class="topbar-device-picker__option"
        data-device-serial={device.serial}
        selected={selectedDevice?.serial === device.serial ? true : undefined}
        typeaheadText={`${deviceName} ${device.serial}`}
      >
        <MaterialIcon slot="start" name={isWireless ? 'wifi' : 'smartphone'} />
        <div slot="headline">{deviceName}</div>
        <div slot="supporting-text">{device.serial} · {stateLabel(device.state)}</div>
        
        {#if isWireless}
          <md-icon-button
            slot="end"
            use:disconnectButton={device.serial}
            title={m.topbar_wireless_disconnect()}
          >
            <MaterialIcon name="close" />
          </md-icon-button>
        {:else if selectedDevice?.serial === device.serial}
          <MaterialIcon slot="end" name="check" />
        {/if}
      </md-menu-item>
    {/each}
  </md-menu>
</div>
