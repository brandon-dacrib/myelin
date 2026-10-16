import { describe, expect, it, vi } from "vitest";
import {
  RELOAD_AT_KEY,
  RELOAD_GUARD_MS,
  isChunkLoadError,
  reloadOnNewBuild,
  type ReloadDeps,
} from "./chunk-reload";

function fakeDeps(start: number) {
  const store = new Map<string, string>();
  const state = { now: start };
  const reload = vi.fn();
  const deps: ReloadDeps = {
    storage: {
      getItem: (k) => store.get(k) ?? null,
      setItem: (k, v) => void store.set(k, v),
    },
    reload,
    now: () => state.now,
  };
  return { deps: () => deps, reload, state, store };
}

const safari = () => Promise.reject(new TypeError("Importing a module script failed."));

describe("isChunkLoadError", () => {
  it("knows every browser's wording and nothing else", () => {
    for (const message of [
      "Failed to fetch dynamically imported module: https://x/admin/assets/UsersPage-a.js",
      "error loading dynamically imported module: https://x/a.js",
      "Importing a module script failed.",
      "Unable to preload CSS for /admin/assets/a.css",
    ]) {
      expect(isChunkLoadError(new TypeError(message))).toBe(true);
    }
    expect(isChunkLoadError(new Error("HTTP 500"))).toBe(false);
    expect(isChunkLoadError("Importing a module script failed.")).toBe(false);
  });
});

describe("reloadOnNewBuild", () => {
  it("passes a loaded module through untouched", async () => {
    const { deps, reload } = fakeDeps(1_000_000);
    await expect(reloadOnNewBuild(() => Promise.resolve({ Page: 1 }), deps)()).resolves.toEqual({
      Page: 1,
    });
    expect(reload).not.toHaveBeenCalled();
  });

  it("reloads on a missing chunk, and again after the next upgrade, even with Safari's identical message", async () => {
    const { deps, reload, state, store } = fakeDeps(1_000_000);
    const load = reloadOnNewBuild(safari, deps);
    let settled = false;
    void load().then(
      () => (settled = true),
      () => (settled = true),
    );
    await Promise.resolve();
    await Promise.resolve();
    expect(reload).toHaveBeenCalledTimes(1);
    expect(settled).toBe(false);
    expect(store.get(RELOAD_AT_KEY)).toBe("1000000");

    // The next release, hours later in the same tab: the same message, reloaded again.
    state.now += 3 * 60 * 60 * 1000;
    void load();
    await Promise.resolve();
    await Promise.resolve();
    expect(reload).toHaveBeenCalledTimes(2);
  });

  it("shows a failure that follows its own reload instead of looping", async () => {
    const { deps, reload, state, store } = fakeDeps(1_000_000);
    store.set(RELOAD_AT_KEY, String(state.now - RELOAD_GUARD_MS + 1));
    await expect(reloadOnNewBuild(safari, deps)()).rejects.toThrow("Importing a module script");
    expect(reload).not.toHaveBeenCalled();
  });

  it("passes other errors on without reloading", async () => {
    const { deps, reload } = fakeDeps(1_000_000);
    const load = reloadOnNewBuild(() => Promise.reject(new Error("boom")), deps);
    await expect(load()).rejects.toThrow("boom");
    expect(reload).not.toHaveBeenCalled();
  });

  it("still reloads when storage refuses", async () => {
    const reload = vi.fn();
    const deps = (): ReloadDeps => ({
      storage: {
        getItem: () => {
          throw new Error("denied");
        },
        setItem: () => {
          throw new Error("denied");
        },
      },
      reload,
      now: () => 5,
    });
    void reloadOnNewBuild(safari, deps)();
    await Promise.resolve();
    await Promise.resolve();
    expect(reload).toHaveBeenCalledTimes(1);
  });
});
