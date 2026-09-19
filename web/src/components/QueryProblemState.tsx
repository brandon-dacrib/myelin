/**
 * Picks the right shared empty/error treatment for a thrown query or mutation error, so no page
 * hand-rolls "is this a 501" logic (docs/status/16-management-web-interface.md, "Degrade
 * honestly": build the treatment once, in the API layer plus one component, not per page).
 *
 * Every page that renders a query's error branch should use this instead of a bare `ErrorState`:
 *
 * ```tsx
 * {isError && <QueryProblemState error={error} resource="users" onRetry={refetch} />}
 * ```
 */
import { classifyError } from "@/api/problem";
import {
  ErrorState,
  ForbiddenState,
  NotImplementedState,
} from "@/components/ui/error-state/ErrorState";

export interface QueryProblemStateProps {
  /** The error thrown by the query/mutation (react-query's `error`), typically an `ApiProblemError`. */
  error: unknown;
  /** What this page was trying to show, e.g. "users", "this bridge", "federation destinations". */
  resource?: string;
  /** Fallback scope name if the `Problem` body doesn't carry `required_scope` (older servers/mocks). */
  scope?: string;
  onRetry?: () => void;
  /** Smaller treatment for one section of an otherwise-loaded page (e.g. a dashboard tile),
   * rather than a full-page state — see `ErrorState`'s `compact` prop. */
  compact?: boolean;
  className?: string;
}

export function QueryProblemState({
  error,
  resource,
  scope,
  onRetry,
  compact,
  className,
}: QueryProblemStateProps) {
  const { kind, problem } = classifyError(error);
  const subject = resource ? capitalize(resource) : "This";

  switch (kind) {
    case "not-implemented":
      return (
        <NotImplementedState
          resource={subject}
          problem={toProblemDetail(problem)}
          compact={compact}
          className={className}
        />
      );
    case "unavailable":
      return (
        <NotImplementedState
          resource={subject}
          problem={toProblemDetail(problem)}
          variant="unavailable"
          onRetry={onRetry}
          compact={compact}
          className={className}
        />
      );
    case "forbidden":
      return (
        <ForbiddenState
          scope={problem?.required_scope ?? scope ?? "an admin"}
          compact={compact}
          className={className}
        />
      );
    case "unauthorized":
      return (
        <ErrorState
          title="Your session has expired"
          problem={{ detail: "Sign in again to continue." }}
          onRetry={onRetry}
          compact={compact}
          className={className}
        />
      );
    case "not-found":
      return (
        <ErrorState
          title={`${subject} not found`}
          problem={toProblemDetail(problem)}
          onRetry={onRetry}
          compact={compact}
          className={className}
        />
      );
    default:
      return (
        <ErrorState
          title={`Couldn't load ${resource ?? "this"}`}
          problem={toProblemDetail(problem)}
          onRetry={onRetry}
          compact={compact}
          className={className}
        />
      );
  }
}

function toProblemDetail(problem?: {
  type?: string;
  title?: string;
  detail?: string;
  request_id?: string;
}) {
  if (!problem) return undefined;
  return {
    type: problem.type,
    title: problem.title,
    detail: problem.detail,
    requestId: problem.request_id,
  };
}

function capitalize(s: string): string {
  return s.length > 0 ? s.charAt(0).toUpperCase() + s.slice(1) : s;
}
