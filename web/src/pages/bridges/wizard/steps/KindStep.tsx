import { useBridgeTypes, type BridgeType } from "@/api/bridges";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { ErrorState } from "@/components/ui/error-state/ErrorState";
import { cn } from "@/lib/cn";

export function KindStep({
  selected,
  onSelect,
}: {
  selected: string;
  onSelect: (kindId: string, kind: BridgeType) => void;
}) {
  const { data: kinds, isLoading, isError, refetch } = useBridgeTypes();

  if (isLoading) return <SkeletonText lines={6} />;
  if (isError) return <ErrorState title="Couldn't load bridge types" onRetry={() => refetch()} />;

  return (
    <div>
      <h2 className="text-lg text-text">What are you connecting?</h2>
      <p className="mt-1 text-sm text-text-muted">
        Choosing a type fills sensible defaults for every later step.
      </p>
      <div
        role="radiogroup"
        aria-label="Bridge type"
        className="mt-6 grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3"
      >
        {kinds?.map((kind) => (
          <button
            key={kind.id}
            type="button"
            role="radio"
            aria-checked={selected === kind.id}
            onClick={() => onSelect(kind.id ?? "", kind)}
            className={cn(
              "flex flex-col items-start gap-1 rounded-md border p-4 text-left",
              selected === kind.id
                ? "border-accent bg-accent-muted"
                : "border-border bg-surface hover:bg-surface-sunken",
              "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
            )}
          >
            <span className="text-sm font-medium text-text">{kind.name}</span>
            <span className="text-xs text-text-muted">{kind.upstream_project}</span>
            {kind.config_keys && kind.config_keys.length > 0 && (
              <span className="mt-1 text-xs text-text-faint">
                Needs:{" "}
                {kind.config_keys
                  .filter((k) => k.required)
                  .map((k) => k.description ?? k.key)
                  .join(", ")}
              </span>
            )}
            {kind.supports_double_puppeting && (
              <span className="mt-1 text-xs text-text-faint">Supports double puppeting</span>
            )}
          </button>
        ))}
      </div>
    </div>
  );
}
