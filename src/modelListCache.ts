import { shallowRef } from "vue";
import { api, type ModelEntry } from "./api";

/**
 * The model directory can contain large sharded safetensors packages. Keep the
 * last inventory alive across Settings tab unmounts and reconcile it quietly
 * in the background instead of blocking every visit with a full rescan.
 */
export const modelEntries = shallowRef<ModelEntry[]>([]);

let loaded = false;
let lastCheckedAt = 0;
let inFlight: Promise<ModelEntry[]> | null = null;
let forcedRefresh: Promise<ModelEntry[]> | null = null;
let backgroundTimer: number | null = null;
const CHECK_INTERVAL_MS = 30_000;

function sameEntry(a: ModelEntry, b: ModelEntry): boolean {
  return a.path === b.path
    && a.name === b.name
    && a.file === b.file
    && a.size === b.size
    && a.size_label === b.size_label
    && a.mtime === b.mtime
    && a.mtime_label === b.mtime_label
    && a.kind === b.kind
    && a.kind_label === b.kind_label
    && a.active === b.active
    && a.format === b.format
    && a.files_complete === b.files_complete
    && a.direct_loadable === b.direct_loadable
    && a.model_dir === b.model_dir
    && a.modalities.join("\0") === b.modalities.join("\0")
    && a.candidate_backends.join("\0") === b.candidate_backends.join("\0");
}

function reconcile(next: ModelEntry[]) {
  const previous = new Map(modelEntries.value.map((entry) => [entry.path, entry]));
  const reconciled = next.map((entry) => {
    const old = previous.get(entry.path);
    return old && sameEntry(old, entry) ? old : entry;
  });
  // A fresh array removes deleted files; entries with the same metadata retain
  // object identity so Vue does not redraw unaffected model rows.
  modelEntries.value = reconciled;
  loaded = true;
}

function startBackgroundChecks() {
  if (backgroundTimer !== null || typeof window === "undefined") return;
  backgroundTimer = window.setInterval(() => {
    void refreshModelList(false).catch(() => {
      // Background inventory checks are intentionally silent. The current
      // cached list remains usable and the next interval retries.
    });
  }, Math.floor(CHECK_INTERVAL_MS / 2));
}

/** Load the inventory once, then refresh it no more often than every 30s. */
export function refreshModelList(force = false): Promise<ModelEntry[]> {
  startBackgroundChecks();
  if (inFlight) {
    if (!force) return inFlight;
    if (!forcedRefresh) {
      const current = inFlight;
      forcedRefresh = current
        .catch(() => modelEntries.value)
        .then(() => {
          forcedRefresh = null;
          return refreshModelList(true);
        });
    }
    return forcedRefresh;
  }
  if (loaded && !force && Date.now() - lastCheckedAt < CHECK_INTERVAL_MS) {
    return Promise.resolve(modelEntries.value);
  }

  inFlight = api.modelList()
    .then((entries) => {
      reconcile(entries);
      lastCheckedAt = Date.now();
      return modelEntries.value;
    })
    .finally(() => {
      inFlight = null;
    });
  return inFlight;
}

export function hasModelListCache(): boolean {
  return loaded;
}
