import { Fragment, useState, type ReactNode } from "react";
import { ChevronDown, ChevronUp, ChevronsUpDown, ChevronRight } from "lucide-react";
import { cn } from "@/lib/cn";
import { SkeletonTableRows } from "../skeleton/Skeleton";

export type SortDirection = "asc" | "desc";

export interface Column<T> {
  key: string;
  header: string;
  render: (row: T) => ReactNode;
  /** Plain-text render for the tablet expansion row and the <768px card fallback. */
  renderCompact?: (row: T) => ReactNode;
  sortable?: boolean;
  /** 1 = always shown, 2 = hidden 768-1023px, 3 = hidden below 1280px (states-density-responsiveness.md #4). */
  priority?: 1 | 2 | 3;
  align?: "start" | "end";
  widthClassName?: string;
  /**
   * Set this when `render`'s output contains its own focusable control
   * (a link, a button, a select...). The <768px card fallback needs to know,
   * because it wraps priority-1 columns in a single tap target: nesting a
   * real `<button>`/`<a>` inside that would be invalid HTML (interactive
   * content is not permitted inside `<button>`) and unreliable for
   * assistive technology and keyboard users. Interactive columns render
   * outside that tap target instead. See DataTable.test.tsx.
   */
  interactive?: boolean;
}

export interface SortState {
  key: string;
  direction: SortDirection;
}

export interface DataTableProps<T> {
  columns: Column<T>[];
  rows: T[];
  getRowId: (row: T) => string;
  caption: string;
  sort?: SortState;
  onSortChange?: (sort: SortState) => void;
  /**
   * Drives the <768px card fallback's whole-card tap target (a real
   * `<button>`). The desktop table does *not* attach this to the `<tr>`:
   * an element made clickable and focusable only via a `<tr onClick>` /
   * `tabIndex` / `onKeyDown="Enter"` combination is a link pretending to be
   * one — it does not respond to Space, exposes no role to assistive
   * technology, and cannot be opened in a new tab. Give the row a real
   * link or button in one of its columns (mark that column
   * `interactive: true`) for desktop row activation instead; `onRowClick`
   * is there for the card view and for callers that want it purely as a
   * convenience alongside a real link, not as a replacement for one.
   */
  onRowClick?: (row: T) => void;
  loading?: boolean;
  empty?: ReactNode;
  /** Cursor pagination footer (states-density-responsiveness.md / information-architecture.md #6: cursor lives in the URL). */
  pagination?: {
    hasPrevious: boolean;
    hasNext: boolean;
    onPrevious: () => void;
    onNext: () => void;
    pageLabel?: string;
  };
  density?: "comfortable" | "compact";
  className?: string;
}

function priorityClasses(priority: 1 | 2 | 3 = 1): string {
  if (priority === 1) return "";
  if (priority === 2) return "hidden lg:table-cell";
  return "hidden xl:table-cell";
}

/**
 * The workhorse table: sticky header, sortable columns, cursor pagination,
 * column priority for tablet, an expandable row for the columns tablet hides,
 * and a card fallback below 768px. See states-density-responsiveness.md #3-4.
 */
export function DataTable<T>({
  columns,
  rows,
  getRowId,
  caption,
  sort,
  onSortChange,
  onRowClick,
  loading,
  empty,
  pagination,
  density = "comfortable",
  className,
}: DataTableProps<T>) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const hiddenAtTablet = columns.filter((c) => (c.priority ?? 1) > 1);

  function toggleExpanded(id: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function handleSort(col: Column<T>) {
    if (!col.sortable || !onSortChange) return;
    const direction: SortDirection =
      sort?.key === col.key && sort.direction === "asc" ? "desc" : "asc";
    onSortChange({ key: col.key, direction });
  }

  if (loading) {
    return (
      <div className={cn("rounded-md border border-border bg-surface p-4", className)}>
        <SkeletonTableRows rows={6} cols={columns.length} />
      </div>
    );
  }

  if (rows.length === 0 && empty) {
    return (
      <div className={cn("rounded-md border border-border bg-surface", className)}>{empty}</div>
    );
  }

  return (
    <div
      data-density={density}
      className={cn("overflow-x-auto rounded-md border border-border bg-surface", className)}
    >
      {/* Table, hidden on phones per states-density-responsiveness.md #4 (cards take over below 768px). */}
      <table className="hidden w-full border-collapse md:table">
        <caption className="sr-only">{caption}</caption>
        <thead className="sticky top-0 z-10 bg-surface">
          <tr className="border-b border-border">
            {hiddenAtTablet.length > 0 && (
              <th scope="col" className="w-10 lg:hidden">
                <span className="sr-only">Expand row</span>
              </th>
            )}
            {columns.map((col) => (
              <th
                key={col.key}
                scope="col"
                aria-sort={
                  sort?.key === col.key
                    ? sort.direction === "asc"
                      ? "ascending"
                      : "descending"
                    : col.sortable
                      ? "none"
                      : undefined
                }
                className={cn(
                  "px-4 py-2 text-xs font-medium uppercase tracking-wide text-text-muted",
                  col.align === "end" ? "text-right" : "text-left",
                  priorityClasses(col.priority),
                  col.widthClassName,
                )}
              >
                {col.sortable ? (
                  <button
                    type="button"
                    onClick={() => handleSort(col)}
                    className={cn(
                      "inline-flex items-center gap-1 rounded-xs hover:text-text",
                      "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
                    )}
                  >
                    {col.header}
                    {sort?.key === col.key ? (
                      sort.direction === "asc" ? (
                        <ChevronUp size={12} aria-hidden="true" />
                      ) : (
                        <ChevronDown size={12} aria-hidden="true" />
                      )
                    ) : (
                      <ChevronsUpDown size={12} aria-hidden="true" className="opacity-50" />
                    )}
                  </button>
                ) : (
                  col.header
                )}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const id = getRowId(row);
            const isExpanded = expanded.has(id);
            return (
              <Fragment key={id}>
                <tr
                  className={cn(
                    "group border-b border-border last:border-b-0",
                    "[height:var(--row-height)] text-[var(--row-text)] leading-[var(--row-text-line-height)]",
                    // Hover affordance only: the row itself is not a control
                    // (see the onRowClick doc comment above). Activation is
                    // whatever real link/button an `interactive` column
                    // renders.
                    onRowClick && "hover:bg-surface-sunken",
                  )}
                >
                  {hiddenAtTablet.length > 0 && (
                    <td className="px-2 lg:hidden">
                      <button
                        type="button"
                        aria-expanded={isExpanded}
                        aria-label={isExpanded ? "Collapse row details" : "Expand row details"}
                        onClick={(e) => {
                          e.stopPropagation();
                          toggleExpanded(id);
                        }}
                        className="rounded-xs p-1 text-text-muted hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
                      >
                        <ChevronRight
                          size={14}
                          aria-hidden="true"
                          className={cn(
                            "transition-transform duration-fast",
                            isExpanded && "rotate-90",
                          )}
                        />
                      </button>
                    </td>
                  )}
                  {columns.map((col) => (
                    <td
                      key={col.key}
                      className={cn(
                        "px-4 py-2",
                        col.align === "end" && "text-right tabular-nums",
                        priorityClasses(col.priority),
                      )}
                    >
                      {col.render(row)}
                    </td>
                  ))}
                </tr>
                {hiddenAtTablet.length > 0 && isExpanded && (
                  <tr className="border-b border-border bg-surface-sunken lg:hidden">
                    <td colSpan={columns.length + 1} className="px-6 py-3">
                      <dl className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm">
                        {hiddenAtTablet.map((col) => (
                          <div key={col.key}>
                            <dt className="text-text-muted">{col.header}</dt>
                            <dd className="text-text">{(col.renderCompact ?? col.render)(row)}</dd>
                          </div>
                        ))}
                      </dl>
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>

      {/* Card fallback below 768px. */}
      <ul className="divide-y divide-border md:hidden">
        {rows.map((row) => {
          const id = getRowId(row);
          const primary = columns.filter((c) => (c.priority ?? 1) === 1);
          // Interactive columns (links, buttons: e.g. an actions column)
          // render outside the tap-target button below, never inside it —
          // a <button> cannot legally contain another <button> or an <a>,
          // and doing so leaves the inner control unreliable for assistive
          // technology and keyboard users (the defect this fixed).
          const staticCols = primary.filter((c) => !c.interactive);
          const interactiveCols = primary.filter((c) => c.interactive);
          const fields = (cols: Column<T>[]) =>
            cols.map((col) => (
              <div key={col.key} className="flex items-center justify-between gap-2 text-sm">
                <span className="text-text-muted">{col.header}</span>
                <span className="text-text">{(col.renderCompact ?? col.render)(row)}</span>
              </div>
            ));
          return (
            <li key={id} className="px-4 py-3">
              {onRowClick ? (
                <button
                  type="button"
                  onClick={() => onRowClick(row)}
                  className="flex w-full flex-col gap-1 text-left focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
                >
                  {fields(staticCols)}
                </button>
              ) : (
                <div className="flex flex-col gap-1">{fields(staticCols)}</div>
              )}
              {interactiveCols.length > 0 && (
                <div className="mt-2 flex flex-col gap-1 border-t border-border pt-2">
                  {fields(interactiveCols)}
                </div>
              )}
            </li>
          );
        })}
      </ul>

      {pagination && (
        <div className="flex items-center justify-between border-t border-border px-4 py-2 text-sm text-text-muted">
          <button
            type="button"
            disabled={!pagination.hasPrevious}
            onClick={pagination.onPrevious}
            className="rounded-sm px-2 py-1 hover:bg-surface-sunken disabled:pointer-events-none disabled:opacity-40 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
          >
            Previous
          </button>
          {pagination.pageLabel && <span>{pagination.pageLabel}</span>}
          <button
            type="button"
            disabled={!pagination.hasNext}
            onClick={pagination.onNext}
            className="rounded-sm px-2 py-1 hover:bg-surface-sunken disabled:pointer-events-none disabled:opacity-40 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
          >
            Next
          </button>
        </div>
      )}
    </div>
  );
}
