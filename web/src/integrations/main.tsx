import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { queryClient } from "@/lib/query-client";
import { IntegrationsApp } from "./IntegrationsApp";
import "@/styles/index.css";

/**
 * The self-service integrations page, served by the homeserver at `/integrations/` (decision
 * 0057): a second entry of the same build as the admin page, with the same design system,
 * signed in with a person's own Matrix account rather than an administrator's.
 */
async function enableMocking(): Promise<void> {
  if (import.meta.env.VITE_HS_MOCK !== "1") return;
  const { worker } = await import("@/mocks/browser");
  await worker.start({
    serviceWorker: { url: "/admin/mockServiceWorker.js" },
    onUnhandledRequest: "bypass",
  });
}

enableMocking().then(() => {
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <QueryClientProvider client={queryClient}>
        <IntegrationsApp />
      </QueryClientProvider>
    </StrictMode>,
  );
});
