import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { router } from "./routes";
import { queryClient } from "./lib/query-client";
import "./styles/index.css";

async function enableMocking(): Promise<void> {
  if (import.meta.env.VITE_HS_MOCK !== "1") return;
  const { worker } = await import("./mocks/browser");
  await worker.start({
    serviceWorker: { url: "/admin/mockServiceWorker.js" },
    onUnhandledRequest: "bypass",
  });
}

enableMocking().then(() => {
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <QueryClientProvider client={queryClient}>
        <RouterProvider router={router} />
      </QueryClientProvider>
    </StrictMode>,
  );
});
