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

export interface TestRoute {
  path: string;
  component?: RouteComponent;
  validateSearch?: (search: Record<string, unknown>) => object;
}

/**
 * Renders `initialPath` in a memory router that knows `routes` (with their components and
 * search validators) and, as blank pages, every path in `known` that a page links to. Test-only.
 */
export function renderRoutes(
  routes: TestRoute[],
  initialPath: string,
  known: string[] = [],
): { router: AnyRouter; client: QueryClient } {
  const rootRoute = createRootRoute();
  const own = new Set(routes.map((r) => r.path));
  const router = createRouter({
    routeTree: rootRoute.addChildren([
      ...routes.map((r) =>
        createRoute({
          getParentRoute: () => rootRoute,
          path: r.path,
          component: r.component ?? (() => <p>{r.path}</p>),
          validateSearch: r.validateSearch,
        }),
      ),
      ...known
        .filter((p) => !own.has(p))
        .map((path) =>
          createRoute({
            getParentRoute: () => rootRoute,
            path,
            component: () => <p>{`page ${path}`}</p>,
          }),
        ),
    ]),
    history: createMemoryHistory({ initialEntries: [initialPath] }),
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { router, client };
}
