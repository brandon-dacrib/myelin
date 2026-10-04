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
 * Renders one bridges page at `path` inside a memory router that also knows the pages it links
 * and navigates to, so a `Link` or `navigate` from it resolves. Test-only.
 */
export function renderBridgesRoute(
  routePath: string,
  Component: RouteComponent,
  initialPath: string,
): { router: AnyRouter } {
  const rootRoute = createRootRoute();
  const blank = (path: string) =>
    createRoute({ getParentRoute: () => rootRoute, path, component: () => <p>{path}</p> });
  const known = [
    "/bridges",
    "/bridges/new",
    "/bridges/registrations",
    "/bridges/offerings/$type",
    "/bridges/$bridgeId",
  ].filter((p) => p !== routePath);
  const router = createRouter({
    routeTree: rootRoute.addChildren([
      createRoute({ getParentRoute: () => rootRoute, path: routePath, component: Component }),
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
