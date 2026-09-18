import { QueryClient } from "@tanstack/react-query";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // "Live by default" (information-architecture.md #5): the SSE stream
      // (once 15 ships it) invalidates queries on change; until then, a
      // short staleTime plus refetchInterval below approximates it without
      // hammering the mock server.
      staleTime: 10_000,
      retry: 1,
    },
  },
});
