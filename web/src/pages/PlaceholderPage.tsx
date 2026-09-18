import { Construction } from "lucide-react";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";

/**
 * The route exists (so the sidebar and navigation match the full information
 * architecture) but the page itself is Phase 1/2 work per
 * docs/workstreams/16-management-web-interface.md; this task built the
 * dashboard and bridges pages only. See docs/status/16-management-web-interface.md.
 */
export function PlaceholderPage({ title }: { title: string }) {
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
