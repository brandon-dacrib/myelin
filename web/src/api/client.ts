import createClient from "openapi-fetch";
import type { paths } from "./schema";
import { getAccessToken } from "@/lib/auth";

/**
 * The API base URL. Defaults to same-origin `/api/v1` (the interface is
 * served by the homeserver at `/admin/`); a standalone deployment overrides
 * this by writing `window.__HS_ADMIN_CONFIG__` from a `config.json` fetched
 * before the app mounts (see docs/design/information-architecture.md #9.1).
 */
function apiBaseUrl(): string {
  const injected = (window as { __HS_ADMIN_CONFIG__?: { apiBaseUrl?: string } }).__HS_ADMIN_CONFIG__
    ?.apiBaseUrl;
  return injected ?? "/api/v1";
}

export const api = createClient<paths>({ baseUrl: apiBaseUrl() });

api.use({
  onRequest({ request }) {
    const token = getAccessToken();
    if (token) {
      request.headers.set("Authorization", `Bearer ${token}`);
    }
    return request;
  },
});

/** A fresh idempotency key for a single mutation attempt (retries reuse it). */
export function newIdempotencyKey(): string {
  return crypto.randomUUID();
}
