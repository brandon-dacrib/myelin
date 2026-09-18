import { AlertTriangle, ShieldOff } from "lucide-react";
import { Button } from "../button/Button";

export interface ProblemDetail {
  type?: string;
  title?: string;
  detail?: string;
  requestId?: string;
}

export interface ErrorStateProps {
  title?: string;
  problem?: ProblemDetail;
  onRetry?: () => void;
  className?: string;
}

/** 5xx, network, parse errors. Plain-language title, the problem detail, request ID, Retry. */
export function ErrorState({ title, problem, onRetry, className }: ErrorStateProps) {
  return (
    <div
      role="alert"
      className={`flex flex-col items-center gap-3 py-16 text-center ${className ?? ""}`}
    >
      <AlertTriangle aria-hidden="true" className="size-8 text-danger" />
      <p className="text-md font-medium text-text">
        {title ?? problem?.title ?? "Something went wrong"}
      </p>
      {problem?.detail && <p className="max-w-sm text-sm text-text-muted">{problem.detail}</p>}
      {onRetry && (
        <Button variant="secondary" onClick={onRetry}>
          Retry
        </Button>
      )}
      {problem?.requestId && (
        <p className="text-xs text-text-faint">Request ID: {problem.requestId}</p>
      )}
    </div>
  );
}

export interface ForbiddenStateProps {
  scope: string;
  className?: string;
}

/** 403 or missing scope: never a generic error (information-architecture.md #8). */
export function ForbiddenState({ scope, className }: ForbiddenStateProps) {
  return (
    <div
      role="alert"
      className={`flex flex-col items-center gap-3 py-16 text-center ${className ?? ""}`}
    >
      <ShieldOff aria-hidden="true" className="size-8 text-text-faint" />
      <p className="text-md font-medium text-text">
        This needs the <code className="font-identifier">{scope}</code> scope.
      </p>
      <p className="max-w-sm text-sm text-text-muted">Ask an administrator to grant it.</p>
    </div>
  );
}
