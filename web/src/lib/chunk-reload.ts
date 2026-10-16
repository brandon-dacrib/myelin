/**
 * A page's code chunk that is gone because the server was upgraded under an open tab.
 *
 * Every build names its chunks by content hash, and a server serves only its own build's
 * chunks. A tab loaded before an upgrade still runs the old `index` script, which asks for the
 * old chunk names when the person opens a page they have not visited yet: a 404, which the
 * browser reports as a failed dynamic import. The only cure is to load the new build, so the
 * page reloads itself.
 *
 * TanStack Router's `lazyRouteComponent` reloads too, but once per tab session per error
 * message. Safari's message ("Importing a module script failed.") names no chunk, so after one
 * upgrade's reload the next upgrade's failure in the same tab was shown instead of reloaded
 * (the demo, 2026-10-16, Users after the 0.3.31 roll). Here the guard is time, not the message:
 * a failure reloads unless this tab reloaded for one in the last [`RELOAD_GUARD_MS`]; a failure
 * right after a reload is a real one (a chunk the new build lacks, a server that is down), and
 * shows as the error it is instead of reloading in a loop.
 */

/** How long after a reload for a missing chunk another failure is shown rather than reloaded. */
export const RELOAD_GUARD_MS = 30_000;

/** The `sessionStorage` key holding when this tab last reloaded for a missing chunk. */
export const RELOAD_AT_KEY = "myelin:chunk-reload-at";

/**
 * Whether `error` is a browser's report of a dynamic import or a chunk's preload failing:
 * Chrome's and Edge's "Failed to fetch dynamically imported module", Firefox's "error loading
 * dynamically imported module", Safari's "Importing a module script failed", and Vite's own
 * "Unable to preload CSS" for a stylesheet a chunk depends on.
 */
export function isChunkLoadError(error: unknown): boolean {
  if (!(error instanceof Error)) return false;
  return [
    "Failed to fetch dynamically imported module",
    "error loading dynamically imported module",
    "Importing a module script failed",
    "Unable to preload CSS",
  ].some((prefix) => error.message.startsWith(prefix));
}

/** What [`reloadOnNewBuild`] needs from the browser; the defaults are the real ones. */
export interface ReloadDeps {
  storage: Pick<Storage, "getItem" | "setItem"> | undefined;
  reload: () => void;
  now: () => number;
}

function browserDeps(): ReloadDeps {
  let storage: ReloadDeps["storage"];
  try {
    storage = typeof sessionStorage === "undefined" ? undefined : sessionStorage;
  } catch {
    storage = undefined;
  }
  return {
    storage,
    reload: () => window.location.reload(),
    now: () => Date.now(),
  };
}

/**
 * `importer`, except that a chunk-load failure reloads the page (once per
 * [`RELOAD_GUARD_MS`]) and never settles, so nothing renders the failure while the new build
 * loads. Any other error, or a chunk failure right after such a reload, is passed on.
 */
export function reloadOnNewBuild<T>(
  importer: () => Promise<T>,
  deps: () => ReloadDeps = browserDeps,
): () => Promise<T> {
  return () =>
    importer().catch((error: unknown) => {
      if (!isChunkLoadError(error)) throw error;
      const { storage, reload, now } = deps();
      // When this tab last reloaded for a missing chunk; none recorded (or unreadable) is never.
      let last: number | undefined;
      try {
        const recorded = Number(storage?.getItem(RELOAD_AT_KEY) ?? Number.NaN);
        last = Number.isFinite(recorded) ? recorded : undefined;
      } catch {
        // Storage refused (a private window, blocked site data): reload anyway, unguarded
        // only for as long as storage stays refused, since each reload starts a new page.
      }
      if (last !== undefined && now() - last < RELOAD_GUARD_MS) throw error;
      try {
        storage?.setItem(RELOAD_AT_KEY, String(now()));
      } catch {
        // As above.
      }
      reload();
      return new Promise<T>(() => {});
    });
}
