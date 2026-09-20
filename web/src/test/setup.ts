import "@testing-library/jest-dom/vitest";
import { afterEach, afterAll } from "vitest";
import { cleanup } from "@testing-library/react";
import { server } from "@/mocks/node";

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
server.listen({ onUnhandledRequest: "error" });

afterEach(() => {
  cleanup();
  server.resetHandlers();
});
afterAll(() => server.close());
