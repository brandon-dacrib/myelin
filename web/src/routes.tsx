import {
  createRootRoute,
  createRoute,
  createRouter,
  lazyRouteComponent,
  Link,
  Navigate,
} from "@tanstack/react-router";
import { AppShell } from "@/components/shell/AppShell";
import { WIZARD_STEPS, type WizardStep } from "@/pages/bridges/wizard/wizard-state";

// Route-level code splitting: each page (and its own dependency graph —
// react-query hooks, mock-independent UI, etc.) lands in its own chunk,
// fetched only when its route is visited. AppShell stays a static import:
// it is needed on first paint for every route regardless (nav, top bar,
// theme, command palette), so lazy-loading it would only add a waterfall.
// `lazyRouteComponent` (not plain `React.lazy`) is TanStack Router's own
// wrapper: it also gets `.preload()` on route hover/intent for free.
const DashboardPage = lazyRouteComponent(() => import("@/pages/DashboardPage"), "DashboardPage");
const BridgesListPage = lazyRouteComponent(
  () => import("@/pages/bridges/BridgesListPage"),
  "BridgesListPage",
);
const BridgeDetailPage = lazyRouteComponent(
  () => import("@/pages/bridges/BridgeDetailPage"),
  "BridgeDetailPage",
);
const AddBridgeWizardPage = lazyRouteComponent(
  () => import("@/pages/bridges/wizard/AddBridgeWizardPage"),
  "AddBridgeWizardPage",
);
const BridgeCreatedPage = lazyRouteComponent(
  () => import("@/pages/bridges/wizard/BridgeCreatedPage"),
  "BridgeCreatedPage",
);
const UsersPage = lazyRouteComponent(() => import("@/pages/UsersPage"), "UsersPage");
const UserDetailPage = lazyRouteComponent(() => import("@/pages/UserDetailPage"), "UserDetailPage");
const RoomsPage = lazyRouteComponent(() => import("@/pages/RoomsPage"), "RoomsPage");
const RoomDetailPage = lazyRouteComponent(() => import("@/pages/RoomDetailPage"), "RoomDetailPage");
const FederationPage = lazyRouteComponent(() => import("@/pages/FederationPage"), "FederationPage");
const FederationDestinationPage = lazyRouteComponent(
  () => import("@/pages/FederationDestinationPage"),
  "FederationDestinationPage",
);
const ConfigurationPage = lazyRouteComponent(
  () => import("@/pages/config/ConfigurationPage"),
  "ConfigurationPage",
);
const ConfigSectionPage = lazyRouteComponent(
  () => import("@/pages/config/ConfigSectionPage"),
  "ConfigSectionPage",
);
const PlaceholderPage = lazyRouteComponent(
  () => import("@/pages/PlaceholderPage"),
  "PlaceholderPage",
);

function NotFoundPage() {
  return (
    <div className="p-6">
      <h1 className="text-xl text-text">Not found</h1>
      <p className="mt-2 text-sm text-text-muted">
        Nothing lives at this address.{" "}
        <Link to="/" className="text-accent hover:underline">
          Back to Overview
        </Link>
      </p>
    </div>
  );
}

const rootRoute = createRootRoute({ component: AppShell, notFoundComponent: NotFoundPage });

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: DashboardPage,
});

// `/setup` is rendered by `AppShell` itself while there is no session (see `Setup.tsx`). With a
// session there is nothing to set up, so the address just goes home.
const setupRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/setup",
  component: () => <Navigate to="/" replace />,
});

interface BridgesListSearch {
  state?: string;
  kind?: string;
  cursor?: string;
}

const bridgesListRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges",
  validateSearch: (search: Record<string, unknown>): BridgesListSearch => ({
    state: typeof search.state === "string" ? search.state : undefined,
    kind: typeof search.kind === "string" ? search.kind : undefined,
    cursor: typeof search.cursor === "string" ? search.cursor : undefined,
  }),
  component: BridgesListPage,
});

interface WizardSearch {
  step?: WizardStep;
}

const bridgesNewRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges/new",
  validateSearch: (search: Record<string, unknown>): WizardSearch => ({
    step: WIZARD_STEPS.includes(search.step as WizardStep)
      ? (search.step as WizardStep)
      : undefined,
  }),
  component: AddBridgeWizardPage,
});

const bridgeCreatedRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges/$bridgeId/created",
  component: BridgeCreatedPage,
});

interface BridgeDetailSearch {
  tab?: string;
}

const bridgeDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges/$bridgeId",
  validateSearch: (search: Record<string, unknown>): BridgeDetailSearch => ({
    tab: typeof search.tab === "string" ? search.tab : undefined,
  }),
  component: BridgeDetailPage,
});

interface SearchAndCursor {
  q?: string;
  cursor?: string;
}
function searchAndCursorValidator(search: Record<string, unknown>): SearchAndCursor {
  return {
    q: typeof search.q === "string" ? search.q : undefined,
    cursor: typeof search.cursor === "string" ? search.cursor : undefined,
  };
}

const usersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/users",
  validateSearch: searchAndCursorValidator,
  component: UsersPage,
});
const userDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/users/$userId",
  component: UserDetailPage,
});
const roomsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/rooms",
  validateSearch: searchAndCursorValidator,
  component: RoomsPage,
});
const roomDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/rooms/$roomId",
  component: RoomDetailPage,
});
const federationRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/federation",
  component: FederationPage,
});
const federationDestinationRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/federation/$serverName",
  component: FederationDestinationPage,
});
const configurationRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/configuration",
  component: ConfigurationPage,
});
const configSectionRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/configuration/$section",
  component: ConfigSectionPage,
});

// Sections on the information architecture (docs/design/information-architecture.md
// #3) that this task did not build pages for; the route exists so navigation,
// the sidebar and deep links match the full IA. See docs/status/16-management-web-interface.md.
function placeholderRoute<T extends string>(path: T) {
  return createRoute({ getParentRoute: () => rootRoute, path, component: PlaceholderPage });
}

const reportsRoute = placeholderRoute("/reports");
const mediaRoute = placeholderRoute("/media");
const clusterRoute = placeholderRoute("/cluster");
const migrationRoute = placeholderRoute("/migration");
const auditRoute = placeholderRoute("/audit");
const settingsRoute = placeholderRoute("/settings");

const routeTree = rootRoute.addChildren([
  indexRoute,
  setupRoute,
  bridgesListRoute,
  bridgesNewRoute,
  bridgeCreatedRoute,
  bridgeDetailRoute,
  usersRoute,
  userDetailRoute,
  roomsRoute,
  roomDetailRoute,
  reportsRoute,
  federationRoute,
  federationDestinationRoute,
  configurationRoute,
  configSectionRoute,
  mediaRoute,
  clusterRoute,
  migrationRoute,
  auditRoute,
  settingsRoute,
]);

export const router = createRouter({ routeTree, basepath: "/admin" });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
