// Debloat knowledge base (~0.7 MB of JSON). It is only used by the Apps page, so it is loaded the
// first time it is asked for instead of being bundled into the main chunk. `getDebloatInfo`
// returns undefined until it is ready; the loaded state is reactive, so the callers refresh.

export interface DebloatInfo {
  id: string;
  label?: string;
  description: string;
  removal: "delete" | "replace" | "caution" | "unsafe";
  warning?: string;
}

// Entries are either a description or `[description, warning]`.
type DebloatData = Partial<Record<DebloatInfo["removal"], Record<string, string | string[]>>>;

const REMOVAL_MAP = ["delete", "replace", "caution", "unsafe"] as const;

let data = $state.raw<DebloatData | null>(null);
let loading: Promise<void> | null = null;

export function loadDebloatData(): Promise<void> {
  loading ??= import("../assets/debloat-data.json")
    .then(module => {
      data = module.default as unknown as DebloatData;
    })
    .catch(() => {
      // Retried by the next lookup.
      loading = null;
    });
  return loading;
}

export function getDebloatInfo(packageName: string): DebloatInfo | undefined {
  const loaded = data; // reading the state is what makes callers reactive
  if (!loaded) {
    void loadDebloatData();
    return undefined;
  }
  for (const removal of REMOVAL_MAP) {
    const item = loaded[removal]?.[packageName];
    if (item) {
      return {
        id: packageName,
        removal,
        description: typeof item === "string" ? item : item[0],
        warning: typeof item === "string" ? undefined : item[1],
      };
    }
  }
  return undefined;
}
