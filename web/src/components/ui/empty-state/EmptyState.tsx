import type { ReactNode } from "react";
import { cn } from "@/lib/cn";

export interface EmptyStateProps {
  icon: ReactNode;
  title: string;
  description?: string;
  action?: ReactNode;
  docsHref?: string;
  /** Filtered-empty (in-table) is a smaller variant per states-density-responsiveness.md #1. */
  variant?: "page" | "filtered";
  className?: string;
}

export function EmptyState({
  icon,
  title,
  description,
  action,
  docsHref,
  variant = "page",
  className,
}: EmptyStateProps) {
  const isFiltered = variant === "filtered";
  return (
    <div
      role="status"
      className={cn(
        "flex flex-col items-center justify-center text-center",
        isFiltered ? "gap-2 py-8" : "gap-3 py-16",
        className,
      )}
    >
      <div className={cn("text-text-faint", isFiltered ? "[&>svg]:size-6" : "[&>svg]:size-8")}>
        {icon}
      </div>
      <p className={cn("font-medium text-text", isFiltered ? "text-sm" : "text-md")}>{title}</p>
      {description && <p className="max-w-sm text-sm text-text-muted">{description}</p>}
      {(action || docsHref) && (
        <div className="mt-1 flex items-center gap-4">
          {action}
          {docsHref && (
            <a href={docsHref} className="text-sm text-accent hover:underline">
              Learn more
            </a>
          )}
        </div>
      )}
    </div>
  );
}
