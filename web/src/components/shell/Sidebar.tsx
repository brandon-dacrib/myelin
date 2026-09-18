import { Link } from "@tanstack/react-router";
import { navItems } from "./nav";
import { hasScope } from "@/lib/auth";
import { cn } from "@/lib/cn";
import { useAppservices } from "@/api/bridges";
import { useClusterStatus } from "@/api/dashboard";

export interface SidebarProps {
  /** "rail" = icon-only (1024-1279px), "full" = icon + label (>=1280px). */
  variant: "rail" | "full";
  onNavigate?: () => void;
}

export function Sidebar({ variant, onNavigate }: SidebarProps) {
  const { data: appservicePage } = useAppservices({ limit: 50 });
  const { data: cluster } = useClusterStatus();
  const bridgesInError = appservicePage?.items.filter((b) => b.health === "down").length ?? 0;
  // No explicit single-node/cluster boolean on ClusterStatus; replica_count
  // <= 1 is this track's heuristic (api/dashboard.ts's doc comment).
  const singleNode = (cluster?.replica_count ?? 1) <= 1;

  const items = navItems.filter((item) => {
    if (item.id === "migration") return false; // shown only when an import exists (information-architecture.md #3)
    if (item.id === "cluster" && singleNode) return false;
    if (item.scope) return hasScope(item.scope);
    return true;
  });

  return (
    <nav aria-label="Primary" className="flex h-full flex-col gap-1 overflow-y-auto p-2">
      {items.map((item) => {
        const Icon = item.icon;
        const count = item.id === "bridges" ? bridgesInError : 0;
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
    </nav>
  );
}
