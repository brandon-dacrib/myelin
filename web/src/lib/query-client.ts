import { QueryClient } from "@tanstack/react-query";
import { isRetryableError } from "@/api/problem";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // "Live by default" (information-architecture.md #5): the SSE stream
      // (once 15 ships it) invalidates queries on change; until then, a
      // short staleTime plus refetchInterval below approximates it without
      // hammering the mock server.
      staleTime: 10_000,
      // Against a real server, most operations answer 501 (not implemented)
      // or 403 (missing scope) — permanent for the page's lifetime.
      // Retrying those just delays the honest "not implemented yet" answer
      // and risks looking like a stuck spinner (docs/status/
      // 16-management-web-interface.md, "Degrade honestly"). Only a 503 or
      // a generic/network failure gets a couple of retries.
      retry: (failureCount, error) => isRetryableError(error) && failureCount < 2,
    },
  },
});
