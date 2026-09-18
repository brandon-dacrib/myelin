import { createRootRoute, createRoute, createRouter, Link } from "@tanstack/react-router";
import { AppShell } from "@/components/shell/AppShell";
import { DashboardPage } from "@/pages/DashboardPage";
import { BridgesListPage } from "@/pages/bridges/BridgesListPage";
import { BridgeDetailPage } from "@/pages/bridges/BridgeDetailPage";
import { AddBridgeWizardPage } from "@/pages/bridges/wizard/AddBridgeWizardPage";
import { BridgeCreatedPage } from "@/pages/bridges/wizard/BridgeCreatedPage";
import { PlaceholderPage } from "@/pages/PlaceholderPage";
import { WIZARD_STEPS, type WizardStep } from "@/pages/bridges/wizard/wizard-state";

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

// Sections on the information architecture (docs/design/information-architecture.md
// #3) that this task did not build pages for; the route exists so navigation,
// the sidebar and deep links match the full IA. See docs/status/16-management-web-interface.md.
function placeholderRoute<T extends string>(path: T, title: string) {
  return createRoute({
    getParentRoute: () => rootRoute,
    path,
    component: () => <PlaceholderPage title={title} />,
  });
}

const usersRoute = placeholderRoute("/users", "Users");
const roomsRoute = placeholderRoute("/rooms", "Rooms");
const reportsRoute = placeholderRoute("/reports", "Reports");
const federationRoute = placeholderRoute("/federation", "Federation");
const mediaRoute = placeholderRoute("/media", "Media");
const clusterRoute = placeholderRoute("/cluster", "Cluster");
const migrationRoute = placeholderRoute("/migration", "Migration");
const auditRoute = placeholderRoute("/audit", "Audit log");
const settingsRoute = placeholderRoute("/settings", "Settings");

const routeTree = rootRoute.addChildren([
  indexRoute,
  bridgesListRoute,
  bridgesNewRoute,
  bridgeCreatedRoute,
  bridgeDetailRoute,
  usersRoute,
  roomsRoute,
  reportsRoute,
  federationRoute,
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
