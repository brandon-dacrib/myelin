import { useMemo, useState } from "react";
import { ExternalLink, Search } from "lucide-react";
import { useBridgeTypes, type BridgeType } from "@/api/bridges";
import { BridgeGlyph } from "@/components/BridgeGlyph";
import { Input } from "@/components/ui/input/Input";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { ErrorState } from "@/components/ui/error-state/ErrorState";
import { groupByCategory } from "@/lib/bridge-catalogue";
import { cn } from "@/lib/cn";

/**
 * flows.md flow 1 step 2: the catalogue, grouped the way a person thinks about it (what they
 * are connecting, not which project bridges it), each entry with what it does and what it
 * needs. The documentation link for the chosen one sits under the grid rather than inside the
 * card: a card is a radio button, and a link inside a button is not valid HTML.
 */
export function KindStep({
  selected,
  onSelect,
}: {
  selected: string;
  onSelect: (kindId: string, kind: BridgeType) => void;
}) {
  const { data: kinds, isLoading, isError, refetch } = useBridgeTypes();
  const [query, setQuery] = useState("");
  const groups = useMemo(() => groupByCategory(kinds ?? [], query), [kinds, query]);
  const chosen = kinds?.find((k) => k.id === selected);

  if (isLoading) return <SkeletonText lines={6} />;
  if (isError) return <ErrorState title="Couldn't load bridge types" onRetry={() => refetch()} />;

  return (
    <div>
      <h2 className="text-lg text-text">What are you connecting?</h2>
      <p className="mt-1 text-sm text-text-muted">
        Choosing a network fills sensible defaults for every later step. Signing in happens later,
        from a chat with the bridge&apos;s bot; the last page tells you exactly how.
      </p>

      <div className="relative mt-5 max-w-sm">
        <Search
          size={16}
          aria-hidden="true"
          className="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-text-faint"
        />
        <Input
          type="search"
          aria-label="Search bridges"
          placeholder="Search bridges"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          className="pl-9"
        />
      </div>

      {groups.length === 0 && (
        <p className="mt-6 text-sm text-text-muted">No bridge matches &ldquo;{query}&rdquo;.</p>
      )}

      {groups.map((group) => (
        <section key={group.category} aria-labelledby={`kind-${group.category}`} className="mt-6">
          <div className="flex items-baseline gap-2">
            <h3
              id={`kind-${group.category}`}
              className="text-xs font-medium tracking-wide text-text-muted uppercase"
            >
              {group.label}
            </h3>
            {group.blurb && <span className="text-xs text-text-faint">{group.blurb}</span>}
          </div>
          <div
            role="radiogroup"
            aria-label={group.label}
            className="mt-2 grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3"
          >
            {group.types.map((kind) => {
              // "Needs a phone with WhatsApp": the catalogue writes descriptions as sentences.
              const needs = (kind.config_keys ?? [])
                .filter((k) => k.required)
                .map((k) => k.description ?? k.key ?? "")
                .map((d) => d.charAt(0).toLowerCase() + d.slice(1))
                .join("; ");
              const isSelected = selected === kind.id;
              return (
                <button
                  key={kind.id}
                  type="button"
                  role="radio"
                  aria-checked={isSelected}
                  onClick={() => onSelect(kind.id ?? "", kind)}
                  className={cn(
                    "flex items-start gap-3 rounded-md border p-4 text-left transition-colors duration-fast",
                    isSelected
                      ? "border-accent bg-accent-muted"
                      : "border-border bg-surface hover:bg-surface-sunken",
                    "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
                  )}
                >
                  <BridgeGlyph category={kind.category} />
                  <span className="flex min-w-0 flex-col gap-1">
                    <span className="text-sm font-medium text-text">{kind.name}</span>
                    {kind.description && (
                      <span className="text-xs text-text-muted">{kind.description}</span>
                    )}
                    {needs && (
                      <span className="text-xs text-text-faint">
                        <span className="font-medium">Needs</span> {needs}
                      </span>
                    )}
                  </span>
                </button>
              );
            })}
          </div>
        </section>
      ))}

      {chosen && (
        <div className="mt-6 flex flex-wrap items-center justify-between gap-2 rounded-md border border-border bg-surface px-4 py-3 text-sm">
          <span className="text-text">
            <span className="font-medium">{chosen.name}</span>
            <span className="text-text-muted"> · {chosen.upstream_project}</span>
          </span>
          {chosen.docs_url && (
            <a
              href={chosen.docs_url}
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-1 text-accent hover:underline"
            >
              <ExternalLink size={14} aria-hidden="true" />
              Documentation
            </a>
          )}
        </div>
      )}
    </div>
  );
}
