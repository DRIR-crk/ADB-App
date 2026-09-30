// Marketing names of Android devices (`{ brand: { model: name } }`, ~1.2 MB of JSON).
// It is only needed to prettify names, so it is loaded after startup instead of being bundled into
// (and parsed with) the main chunk. Lookups return '' until it is ready and, because the loaded
// state is reactive, every template/derived that asked for a name refreshes by itself.

type DeviceDb = Record<string, Record<string, string>>;

let db = $state.raw<DeviceDb | null>(null);
/** model -> "Brand Name", first brand in database order (same result as scanning every brand). */
let firstBrandByModel = new Map<string, string>();
let loading: Promise<void> | null = null;

export function loadDeviceDb(): Promise<void> {
  loading ??= import('../assets/device-db.json')
    .then(module => {
      const data = module.default as DeviceDb;
      const index = new Map<string, string>();
      for (const [brand, models] of Object.entries(data)) {
        for (const [model, name] of Object.entries(models)) {
          if (!index.has(model)) index.set(model, `${brand} ${name}`);
        }
      }
      firstBrandByModel = index;
      db = data;
    })
    .catch(() => {
      // Names simply stay unresolved; a later lookup retries.
      loading = null;
    });
  return loading;
}

function loadWhenIdle() {
  if (loading) return;
  const start = () => void loadDeviceDb();
  if (typeof requestIdleCallback === 'function') requestIdleCallback(start, { timeout: 1500 });
  else setTimeout(start, 300);
}

/** Marketing name for a model ("Google Pixel 10"), or '' when unknown or not loaded yet. */
export function lookupMarketingName(model: string, brand?: string): string {
  const data = db; // reading the state is what makes callers reactive
  if (!data) {
    loadWhenIdle();
    return '';
  }
  if (brand) {
    const name = data[brand]?.[model];
    if (name) return `${brand} ${name}`;
  }
  return firstBrandByModel.get(model) ?? '';
}
