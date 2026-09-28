import { classifyError } from "@/api/problem";
import { cn } from "@/lib/cn";

/**
 * Why a change was refused, in the server's own words where it gave some: a write's failure is
 * about the change, so `QueryProblemState`'s "Couldn't load ..." would be the wrong sentence.
 */
export function MutationError({
  error,
  action,
  className,
}: {
  error: unknown;
  /** What was being attempted, as it reads after "Couldn't": "save the decision". */
  action: string;
  className?: string;
}) {
  const { kind, problem } = classifyError(error);
  const reason =
    kind === "forbidden"
      ? `This needs the ${problem?.required_scope ?? "right"} scope.`
      : kind === "not-implemented"
        ? "This server does not do that yet."
        : (problem?.detail ?? problem?.title ?? "Something went wrong. Try again.");
  return (
    <p
      role="alert"
      className={cn("rounded-sm border border-danger-border bg-danger-bg p-3 text-sm", className)}
    >
      <span className="font-medium text-danger">Couldn&apos;t {action}.</span>{" "}
      <span className="text-text">{reason}</span>
    </p>
  );
}
