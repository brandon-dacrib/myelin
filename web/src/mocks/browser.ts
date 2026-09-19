import { setupWorker } from "msw/browser";
import { http, HttpResponse } from "msw";
import { handlers } from "./handlers";
import { clusterStatus } from "./data/dashboard";
import { queryClient } from "@/lib/query-client";

/** Started from src/main.tsx when VITE_HS_MOCK=1 (npm run dev:mock / build:mock). */
export const worker = setupWorker(...handlers);

/**
 * A small, explicit test seam for Playwright (e2e/utils.ts): some flows
 * (the Kubernetes deployment card, flows.md flow 1 step 4) only appear in
 * cluster mode (`replica_count > 1`, the heuristic in api/dashboard.ts's
 * doc comment), which the default fixtures do not represent. Overriding via
 * `worker.use()` here is reliable where `page.route()` is not, because MSW
 * synthesizes its response inside the service worker without an outgoing
 * network request for Playwright's CDP-level routing to intercept. This
 * only exists when the mock worker is running (VITE_HS_MOCK=1), never in a
 * build served against the real admin API.
 */
declare global {
  interface Window {
    __hsAdminMock?: {
      setClusterMode(mode: "single-node" | "cluster", replicaCount?: number): Promise<void>;
      setForceProblem(
        path: string,
        status: 403 | 501 | 503,
        extra?: { detail?: string; required_scope?: string },
      ): Promise<void>;
    };
  }
}

window.__hsAdminMock = {
  setClusterMode(mode, replicaCount = mode === "cluster" ? 3 : 1) {
    worker.use(
      http.get("/api/v1/cluster", () =>
        HttpResponse.json({ ...clusterStatus, mode, replica_count: replicaCount }),
      ),
    );
    // The cluster-status query is already cached from the initial load with
    // a staleTime (src/lib/query-client.ts); without this, consumers would
    // keep reading the pre-override response until that elapses.
    return queryClient.invalidateQueries({ queryKey: ["cluster-status"] });
  },

  /**
   * Forces `path` (any method, matched by pathname only — MSW ignores query strings unless the
   * pattern itself has one) to answer a real RFC 9457 `Problem` body at `status`, so
   * `e2e/degrade-honestly.spec.ts` can prove `QueryProblemState`/`NotImplementedState`/
   * `ForbiddenState` (`src/components/QueryProblemState.tsx`) render correctly for the statuses
   * the app will see constantly against a real, mostly-unimplemented `hs serve` — without
   * needing one running. Same reasoning as `setClusterMode` for why this is a `worker.use()`
   * override rather than `page.route()`: MSW's service worker never makes an outgoing request
   * for Playwright's CDP-level routing to intercept.
   */
  setForceProblem(path, status, extra) {
    const titleByStatus = { 403: "Forbidden", 501: "Not implemented", 503: "Unavailable" } as const;
    const typeByStatus = {
      403: "urn:hs:problem:forbidden",
      501: "urn:hs:problem:not-implemented",
      503: "urn:hs:problem:unavailable",
    } as const;
    const problem = { type: typeByStatus[status], title: titleByStatus[status], status, ...extra };
    worker.use(http.all(path, () => HttpResponse.json(problem, { status })));
    return queryClient.invalidateQueries();
  },
};
