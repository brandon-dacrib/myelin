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
import { OFFER_STEPS, type OfferStep } from "@/pages/bridges/wizard/offer-state";
import { validateAuditSearch } from "@/pages/audit/audit-search";
import { validateMediaSearch } from "@/pages/media/media-search";
import { validateReportsSearch } from "@/pages/reports/reports-search";
import { validateTasksSearch } from "@/pages/tasks/tasks-search";
import { validateStatisticsSearch } from "@/pages/statistics/statistics-search";
import { validateClusterSearch } from "@/pages/cluster/cluster-search";

// Route-level code splitting: each page (and its own dependency graph —
// react-query hooks, mock-independent UI, etc.) lands in its own chunk,
// fetched only when its route is visited. AppShell stays a static import:
// it is needed on first paint for every route regardless (nav, top bar,
// theme, command palette), so lazy-loading it would only add a waterfall.
// `lazyRouteComponent` (not plain `React.lazy`) is TanStack Router's own
// wrapper: it also gets `.preload()` on route hover/intent for free.
const DashboardPage = lazyRouteComponent(() => import("@/pages/DashboardPage"), "DashboardPage");
const BridgeOfferingsPage = lazyRouteComponent(
  () => import("@/pages/bridges/BridgeOfferingsPage"),
  "BridgeOfferingsPage",
);
const BridgeOfferingPage = lazyRouteComponent(
  () => import("@/pages/bridges/offering/BridgeOfferingPage"),
  "BridgeOfferingPage",
);
const OfferBridgeWizardPage = lazyRouteComponent(
  () => import("@/pages/bridges/wizard/OfferBridgeWizardPage"),
  "OfferBridgeWizardPage",
);
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
const RegistrationTokensPage = lazyRouteComponent(
  () => import("@/pages/settings/RegistrationTokensPage"),
  "RegistrationTokensPage",
);
const ServerNoticesPage = lazyRouteComponent(
  () => import("@/pages/settings/ServerNoticesPage"),
  "ServerNoticesPage",
);
const MediaPage = lazyRouteComponent(() => import("@/pages/media/MediaPage"), "MediaPage");
const AuditPage = lazyRouteComponent(() => import("@/pages/audit/AuditPage"), "AuditPage");
const AuditEntryPage = lazyRouteComponent(
  () => import("@/pages/audit/AuditEntryPage"),
  "AuditEntryPage",
);
const ReportsPage = lazyRouteComponent(() => import("@/pages/reports/ReportsPage"), "ReportsPage");
const ReportDetailPage = lazyRouteComponent(
  () => import("@/pages/reports/ReportDetailPage"),
  "ReportDetailPage",
);
const TasksPage = lazyRouteComponent(() => import("@/pages/tasks/TasksPage"), "TasksPage");
const TaskDetailPage = lazyRouteComponent(
  () => import("@/pages/tasks/TaskDetailPage"),
  "TaskDetailPage",
);
const ClusterPage = lazyRouteComponent(() => import("@/pages/cluster/ClusterPage"), "ClusterPage");
const StatisticsPage = lazyRouteComponent(
  () => import("@/pages/statistics/StatisticsPage"),
  "StatisticsPage",
);

function NotFoundPage() {
  return (
    <div className="p-6">
      <h1 className="text-xl text-text">Not found</h1>
      <p className="mt-2 text-sm text-text-muted">
        Nothing lives at this address.{" "}
        <Link to="/" className="text-accent underline underline-offset-2 hover:no-underline">
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

// `/recover` likewise: the recovery link from `hs recover` opens it (see `Recover.tsx`), and with
// a session there is nobody locked out to recover.
const recoverRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/recover",
  component: () => <Navigate to="/" replace />,
});

// Bridges (RFC 0017): offerings first. `/bridges` lists the bridge types this server offers,
// `/bridges/new` offers another, and `/bridges/offerings/$type` is one offering with each
// person's instance. Every instance is also an appservice registration; the registrations list,
// its detail pages and the register-it-yourself wizard live under `/bridges/registrations` and
// keep their `/bridges/$bridgeId` addresses (audit links, the command palette, the overview).
const bridgesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges",
  component: BridgeOfferingsPage,
});

interface OfferWizardSearch {
  step?: OfferStep;
}

const bridgesNewRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges/new",
  validateSearch: (search: Record<string, unknown>): OfferWizardSearch => ({
    step: OFFER_STEPS.includes(search.step as OfferStep) ? (search.step as OfferStep) : undefined,
  }),
  component: OfferBridgeWizardPage,
});

const bridgeOfferingRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges/offerings/$type",
  component: BridgeOfferingPage,
});

interface BridgesListSearch {
  state?: string;
  kind?: string;
  cursor?: string;
}

const bridgeRegistrationsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges/registrations",
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

const bridgeRegisterRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/bridges/registrations/new",
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

const reportsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/reports",
  validateSearch: validateReportsSearch,
  component: ReportsPage,
});
const reportDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/reports/$reportId",
  validateSearch: validateReportsSearch,
  component: ReportDetailPage,
});
const tasksRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/tasks",
  validateSearch: validateTasksSearch,
  component: TasksPage,
});
const taskDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/tasks/$taskId",
  validateSearch: validateTasksSearch,
  component: TaskDetailPage,
});
const statisticsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/statistics",
  validateSearch: validateStatisticsSearch,
  component: StatisticsPage,
});
const mediaRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/media",
  validateSearch: validateMediaSearch,
  component: MediaPage,
});
const clusterRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/cluster",
  validateSearch: validateClusterSearch,
  component: ClusterPage,
});
const migrationRoute = placeholderRoute("/migration");
const auditRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/audit",
  validateSearch: validateAuditSearch,
  component: AuditPage,
});
const auditEntryRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/audit/$entryId",
  validateSearch: validateAuditSearch,
  component: AuditEntryPage,
});
// Settings (information-architecture.md, Settings): one view per kind of thing, each at its own
// address. `/settings` itself goes to the first.
const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings",
  component: () => <Navigate to="/settings/registration-tokens" replace />,
});
interface CursorSearch {
  cursor?: string;
}
function cursorValidator(search: Record<string, unknown>): CursorSearch {
  return { cursor: typeof search.cursor === "string" ? search.cursor : undefined };
}
const registrationTokensRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings/registration-tokens",
  validateSearch: cursorValidator,
  component: RegistrationTokensPage,
});
const serverNoticesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings/server-notices",
  validateSearch: cursorValidator,
  component: ServerNoticesPage,
});

// `/register` is the page an invite link opens (`Register.tsx`). `AppShell` renders it in place
// of everything else, with or without a session, so this route only has to exist.
const registerRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/register",
  component: () => null,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  setupRoute,
  recoverRoute,
  bridgesRoute,
  bridgesNewRoute,
  bridgeOfferingRoute,
  bridgeRegistrationsRoute,
  bridgeRegisterRoute,
  bridgeCreatedRoute,
  bridgeDetailRoute,
  usersRoute,
  userDetailRoute,
  roomsRoute,
  roomDetailRoute,
  reportsRoute,
  reportDetailRoute,
  tasksRoute,
  taskDetailRoute,
  statisticsRoute,
  federationRoute,
  federationDestinationRoute,
  configurationRoute,
  configSectionRoute,
  mediaRoute,
  clusterRoute,
  migrationRoute,
  auditRoute,
  auditEntryRoute,
  settingsRoute,
  registrationTokensRoute,
  serverNoticesRoute,
  registerRoute,
]);

export const router = createRouter({ routeTree, basepath: "/admin" });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
