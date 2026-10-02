import "@testing-library/jest-dom/vitest";
import { afterEach, afterAll } from "vitest";
import { cleanup, configure } from "@testing-library/react";
import { server } from "@/mocks/node";
import { resetBridgeOfferings } from "@/mocks/data/bridge-offerings";
import { resetAppserviceSecrets } from "@/mocks/data/appservices";
import { resetRegistrationTokens } from "@/mocks/data/registration-tokens";
import { resetAdminTokens } from "@/mocks/data/admin-tokens";
import { resetServerNotices } from "@/mocks/data/server-notices";
import { resetMedia } from "@/mocks/data/media";
import { resetReports } from "@/mocks/data/reports";
import { resetTasks } from "@/mocks/data/tasks";
import { resetFederationKeys } from "@/mocks/data/federation";
import { resetCluster } from "@/mocks/data/cluster";
import { resetRoomContents } from "@/mocks/data/room-contents";
import { resetMigration } from "@/mocks/data/migration";
import { resetUserModeration } from "@/mocks/data/user-moderation";
import { resetMockEvents } from "@/mocks/data/events";
import { resetConfigHistory } from "@/mocks/data/config";

/**
 * Give the API client an absolute base URL before anything imports it.
 *
 * In a browser the app calls same-origin `/api/v1` and the relative path is
 * resolved by `fetch` itself. Under jsdom there is no such resolution:
 * `openapi-fetch` builds a `new URL(...)` for every request and a bare
 * `/api/v1/...` throws `ERR_INVALID_URL` before MSW ever sees it. This uses
 * the seam the standalone deployment already has (`window.__HS_ADMIN_CONFIG__`,
 * normally written from `config.json` — see `src/api/client.ts`) to point the
 * client at jsdom's own origin, which is exactly where the MSW handlers'
 * relative paths resolve to as well.
 *
 * It has to run here rather than in a test file: `src/api/client.ts` reads the
 * base URL once, at module scope, and `setupFiles` are the only thing that
 * runs before the test module graph is imported.
 */
(window as { __HS_ADMIN_CONFIG__?: { apiBaseUrl?: string } }).__HS_ADMIN_CONFIG__ = {
  apiBaseUrl: `${window.location.origin}/api/v1`,
};

/**
 * Started here, at module scope, rather than from `beforeAll`.
 *
 * MSW intercepts by replacing `globalThis.fetch`, and `openapi-fetch` captures
 * whatever `globalThis.fetch` is when `createClient` runs — which is when
 * `src/api/client.ts` is first imported, i.e. while the test module graph
 * loads. A `beforeAll` hook runs after that, so the typed client would keep a
 * reference to the real `fetch` and every request would leave the process.
 * Setup files are evaluated before the test module graph, so starting the
 * server here is what makes `api.GET(...)` interceptable at all.
 */
/**
 * jsdom has no `ResizeObserver`, and Radix's Switch (through `use-size`) constructs one as soon
 * as it is *checked* -- so a test that only renders a switch passes and one that turns it on
 * throws. Nothing under test depends on a size ever being reported, so observing nothing is an
 * honest stand-in.
 */
if (!("ResizeObserver" in globalThis)) {
  (globalThis as { ResizeObserver?: unknown }).ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}

/**
 * Radix's Select opens on a pointer event and scrolls its chosen item into view, and jsdom has
 * neither pointer capture nor `scrollIntoView`. Without these a test can render a select but not
 * choose from it; with them it drives one exactly as a keyboard or pointer user would.
 */
if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => undefined;
if (!Element.prototype.hasPointerCapture) Element.prototype.hasPointerCapture = () => false;
if (!Element.prototype.releasePointerCapture) {
  Element.prototype.releasePointerCapture = () => undefined;
}

/**
 * `findBy*` and `waitFor` give up after five seconds, not Testing Library's default one. A page
 * test's first render goes through MSW and the query cache, and with sixty files running at once
 * on a loaded machine that alone can take over a second: on 2026-10-01, at a load average of
 * 22-30, `MediaPage` and `TasksPage` failed `npm run check` with "Unable to find role=table"
 * and passed alone. A wait that is longer only makes a real failure slower to report, so this
 * is the ceiling for every async query; `testTimeout` in `vite.config.ts` leaves room for a test
 * that makes several of them.
 */
configure({ asyncUtilTimeout: 5_000 });

server.listen({ onUnhandledRequest: "error" });

afterEach(() => {
  cleanup();
  server.resetHandlers();
  // The mock's bridge offerings are mutable module state (PUT and DELETE change them).
  resetBridgeOfferings();
  resetAppserviceSecrets();
  // So are the registration tokens and the server-notice history.
  resetRegistrationTokens();
  resetAdminTokens();
  resetServerNotices();
  // So is its media (quarantine, protection and the deletions change it).
  resetMedia();
  resetReports();
  resetTasks();
  resetFederationKeys();
  // And its cluster: a drain moves shards and puts a task on the Tasks page.
  resetCluster();
  // And its rooms: purge, delete, aliases and joins change them.
  resetRoomContents();
  // And its migration from Synapse, and the source the configuration names.
  resetMigration();
  // And users' moderation flags, rate limits and support sessions.
  resetUserModeration();
  // And the event streams a test opened.
  resetMockEvents();
  // And each configuration section's per-setting history (saves and reverts add to it).
  resetConfigHistory();
});
afterAll(() => server.close());
