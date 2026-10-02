import { render } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
  type AnyRouter,
  type RouteComponent,
} from "@tanstack/react-router";

/**
 * Renders `Component` at `initialPath` inside a memory router that also knows the Settings views
 * and the pages they link to, so the tabs and any `Link` resolve. Test-only.
 */
export function renderSettingsRoute(
  routePath: string,
  Component: RouteComponent,
  initialPath = routePath,
): { router: AnyRouter } {
  const rootRoute = createRootRoute();
  const validateSearch = (search: Record<string, unknown>) => ({
    cursor: typeof search.cursor === "string" ? search.cursor : undefined,
  });
  const blank = (path: string) =>
    createRoute({ getParentRoute: () => rootRoute, path, component: () => <p>{path}</p> });
  const known = [
    "/",
    "/settings/registration-tokens",
    "/settings/admin-tokens",
    "/settings/server-notices",
    "/users/$userId",
  ].filter((p) => p !== routePath);
  const router = createRouter({
    routeTree: rootRoute.addChildren([
      createRoute({
        getParentRoute: () => rootRoute,
        path: routePath,
        validateSearch,
        component: Component,
      }),
      ...known.map(blank),
    ]),
    history: createMemoryHistory({ initialEntries: [initialPath] }),
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { router };
}
