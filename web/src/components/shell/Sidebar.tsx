import { Link } from "@tanstack/react-router";
import { NAV_GROUP_LABELS, navItems, type NavGroup, type NavItem } from "./nav";
import { hasScope } from "@/lib/auth";
import { cn } from "@/lib/cn";
import { useAppservices } from "@/api/bridges";
import { useClusterStatus, useStatisticsOverview } from "@/api/dashboard";
import { useMigration } from "@/api/migration";

export interface SidebarProps {
  /** "rail" = icon-only (1024-1279px), "full" = icon + label (>=1280px). */
  variant: "rail" | "full";
  onNavigate?: () => void;
}

export function Sidebar({ variant, onNavigate }: SidebarProps) {
  const { data: appservicePage } = useAppservices({ limit: 50 });
  const { data: cluster } = useClusterStatus();
  // Open reports, from the Overview's own counts (information-architecture.md #6: the sidebar
  // counts what needs attention). Only asked for by someone who could read them.
  const { data: overview } = useStatisticsOverview({
    enabled: hasScope("admin:read") && hasScope("moderation:read"),
  });
  const openReports = overview?.pending_reports_count ?? 0;
  const { data: migration } = useMigration();
  // Migration shows when a migration exists, or on a server so new that one is what an operator
  // is likely to do next (information-architecture.md #3; flows.md flow 5, "Discover").
  const showMigration =
    (migration?.status != null && migration.status !== "idle") ||
    (overview?.users_count != null && overview.users_count <= 1);
  const bridgesInError = appservicePage?.items.filter((b) => b.health === "down").length ?? 0;
  // No explicit single-node/cluster boolean on ClusterStatus; replica_count
  // <= 1 is this track's heuristic (api/dashboard.ts's doc comment).
  const singleNode = (cluster?.replica_count ?? 1) <= 1;

  const items = navItems.filter((item) => {
    if (item.id === "migration" && !showMigration) return false;
    if (item.id === "cluster" && singleNode) return false;
    if (item.scope) return hasScope(item.scope);
    return true;
  });

  // Items in sidebar order, with a heading before each group's first item (full width only:
  // the rail is icons, and a heading there would be a word with nothing to say it about).
  const grouped: { group: NavGroup | undefined; items: NavItem[] }[] = [];
  for (const item of items) {
    const last = grouped[grouped.length - 1];
    if (last && last.group === item.group) last.items.push(item);
    else grouped.push({ group: item.group, items: [item] });
  }

  return (
    <nav aria-label="Primary" className="flex h-full flex-col gap-1 overflow-y-auto p-2">
      {grouped.map(({ group, items: groupItems }, groupIndex) => (
        <div
          key={group ?? "top"}
          className="flex flex-col gap-1"
          role={group ? "group" : undefined}
          aria-label={group ? NAV_GROUP_LABELS[group] : undefined}
        >
          {variant === "full" && group && (
            <p
              className={cn(
                "px-3 pb-1 text-xs font-medium tracking-wide text-text-faint uppercase",
                groupIndex > 0 && "pt-3",
              )}
              aria-hidden="true"
            >
              {NAV_GROUP_LABELS[group]}
            </p>
          )}
          {variant === "rail" && group && groupIndex > 0 && (
            <hr className="mx-3 my-1 border-border" aria-hidden="true" />
          )}
          {groupItems.map((item) => {
            const Icon = item.icon;
            const count =
              item.id === "bridges" ? bridgesInError : item.id === "reports" ? openReports : 0;
            return (
              <Link
                key={item.id}
                to={item.href}
                onClick={onNavigate}
                className={cn(
                  "flex items-center gap-3 rounded-sm px-3 py-2 text-sm font-medium text-text-muted",
                  "hover:bg-surface-sunken hover:text-text",
                  "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
                  variant === "rail" && "justify-center px-0 py-2.5",
                )}
                activeOptions={{ exact: item.href === "/" }}
                activeProps={{ className: "bg-accent-muted text-accent" }}
                title={variant === "rail" ? item.label : undefined}
              >
                <Icon size={18} aria-hidden="true" />
                {variant === "full" && <span className="flex-1">{item.label}</span>}
                {variant === "full" && count > 0 && (
                  <span className="rounded-full bg-danger px-1.5 py-0.5 text-xs font-semibold text-danger-text-on">
                    {count}
                  </span>
                )}
                {variant === "rail" && <span className="sr-only">{item.label}</span>}
              </Link>
            );
          })}
        </div>
      ))}
    </nav>
  );
}
