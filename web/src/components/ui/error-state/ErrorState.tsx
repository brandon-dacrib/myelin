import { AlertTriangle, CloudOff, Construction, ShieldOff } from "lucide-react";
import { Button } from "../button/Button";

export interface ProblemDetail {
  type?: string;
  title?: string;
  detail?: string;
  requestId?: string;
}

/** Shared sizing for every state below: full-page (`py-16`, default) or inline within a
 * smaller region such as a dashboard tile or a section of an otherwise-loaded page
 * (`compact`, `py-6`, smaller icon, no detail paragraph — just enough to be honest, not a
 * second full-page treatment nested inside a working page). */
function wrapperClass(compact: boolean | undefined, className: string | undefined): string {
  return `flex flex-col items-center gap-2 text-center ${compact ? "py-6" : "gap-3 py-16"} ${className ?? ""}`;
}

export interface ErrorStateProps {
  title?: string;
  problem?: ProblemDetail;
  onRetry?: () => void;
  compact?: boolean;
  className?: string;
}

/** 5xx, network, parse errors. Plain-language title, the problem detail, request ID, Retry. */
export function ErrorState({ title, problem, onRetry, compact, className }: ErrorStateProps) {
  return (
    <div role="alert" className={wrapperClass(compact, className)}>
      <AlertTriangle aria-hidden="true" className={compact ? "size-5 text-danger" : "size-8 text-danger"} />
      <p className="text-md font-medium text-text">
        {title ?? problem?.title ?? "Something went wrong"}
      </p>
      {problem?.detail && !compact && (
        <p className="max-w-sm text-sm text-text-muted">{problem.detail}</p>
      )}
      {onRetry && (
        <Button variant="secondary" size={compact ? "sm" : "md"} onClick={onRetry}>
          Retry
        </Button>
      )}
      {problem?.requestId && !compact && (
        <p className="text-xs text-text-faint">Request ID: {problem.requestId}</p>
      )}
    </div>
  );
}

export interface ForbiddenStateProps {
  scope: string;
  compact?: boolean;
  className?: string;
}

/** 403 or missing scope: never a generic error (information-architecture.md #8). */
export function ForbiddenState({ scope, compact, className }: ForbiddenStateProps) {
  return (
    <div role="alert" className={wrapperClass(compact, className)}>
      <ShieldOff aria-hidden="true" className={compact ? "size-5 text-text-faint" : "size-8 text-text-faint"} />
      <p className="text-md font-medium text-text">
        This needs the <code className="font-identifier">{scope}</code> scope.
      </p>
      {!compact && <p className="max-w-sm text-sm text-text-muted">Ask an administrator to grant it.</p>}
    </div>
  );
}

export interface NotImplementedStateProps {
  /** What this page was trying to show, e.g. "Reports" or "This bridge's logins". */
  resource?: string;
  problem?: ProblemDetail;
  /**
   * `"not-implemented"` (501: the operation exists in the API but this server hasn't built
   * the handler yet) or `"unavailable"` (503: the handler exists but isn't wired to a data
   * source yet). Deliberately not styled as a fault (see `ErrorState`): neutral icon, no
   * `role="alert"`, no red — this is an honest "not yet", not a bug the operator should worry
   * about (docs/status/16-management-web-interface.md, "Degrade honestly").
   */
  variant?: "not-implemented" | "unavailable";
  onRetry?: () => void;
  compact?: boolean;
  className?: string;
}

/**
 * The shared treatment for a 501 or 503 response: says so, plainly, instead of a spinner that
 * never resolves, a table that implies zero rows, or a red `ErrorState` that looks like a fault.
 * See `src/api/problem.ts::classifyError` and `src/components/QueryProblemState.tsx`, which
 * picks this over `ErrorState` for exactly these two status codes.
 */
export function NotImplementedState({
  resource,
  problem,
  variant = "not-implemented",
  onRetry,
  compact,
  className,
}: NotImplementedStateProps) {
  const subject = resource ?? "This";
  const heading =
    variant === "unavailable"
      ? `${subject} isn't connected to a data source on this server yet`
      : `${subject} isn't implemented on this server yet`;
  const Icon = variant === "unavailable" ? CloudOff : Construction;
  return (
    <div role="status" className={wrapperClass(compact, className)}>
      <Icon aria-hidden="true" className={compact ? "size-5 text-text-faint" : "size-8 text-text-faint"} />
      <p className={compact ? "text-sm font-medium text-text" : "text-md font-medium text-text"}>
        {heading}
      </p>
      {!compact && (
        <p className="max-w-sm text-sm text-text-muted">
          {problem?.detail ??
            "This is a known gap in the server, not a bug in this page. It will start working once the handler ships."}
        </p>
      )}
      {onRetry && (
        <Button variant="ghost" size={compact ? "sm" : "md"} onClick={onRetry}>
          Check again
        </Button>
      )}
      {problem?.requestId && !compact && (
        <p className="text-xs text-text-faint">Request ID: {problem.requestId}</p>
      )}
    </div>
  );
}
