import { Construction } from "lucide-react";
import { useRouterState } from "@tanstack/react-router";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";

const TITLES: Record<string, string> = {
  reports: "Reports",
  media: "Media",
  cluster: "Cluster",
  migration: "Migration",
  audit: "Audit log",
  settings: "Settings",
};

/**
 * The route exists (so the sidebar and navigation match the full information
 * architecture) but the page itself is Phase 1/2 work per
 * docs/workstreams/16-management-web-interface.md. Every remaining
 * placeholder route shares this one component (and its one lazy-loaded
 * chunk, `src/routes.tsx`), deriving its title from the path rather than a
 * prop, since a route `component` takes no custom props.
 */
export function PlaceholderPage() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const segment = pathname.split("/").filter(Boolean).pop() ?? "";
  const title = TITLES[segment] ?? segment.charAt(0).toUpperCase() + segment.slice(1);

  return (
    <div className="p-6">
      <h1 className="text-xl text-text">{title}</h1>
      <div className="mt-8">
        <EmptyState
          icon={<Construction aria-hidden="true" />}
          title="Not built yet"
          description="This section is on the information architecture but has not been implemented in this pass. See docs/status/16-management-web-interface.md for what is next."
        />
      </div>
    </div>
  );
}
