import { useState, type FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import {
  DEFAULT_BURST_COUNT,
  describeRateLimit,
  hasRateLimitOverride,
  useClearUserRateLimit,
  useSetUserRateLimit,
  useUserRateLimit,
  type RateLimitOverride,
} from "@/api/user-moderation";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Field, Input } from "@/components/ui/input/Input";
import { MutationError } from "@/components/MutationError";
import { QueryProblemState } from "@/components/QueryProblemState";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { rateLimitContext, type ServerRateLimits } from "@/lib/rate-limits";
import { settingRowId } from "@/lib/config-model";

type FieldErrors = { rate?: string; burst?: string; other?: unknown };

/**
 * A per-user override of the server's message rate limit (`/users/{user_id}/rate-limit`):
 * tighter for somebody flooding rooms, 0 to exempt a bot. Beside it, the server-wide limit it
 * replaces (the answer's `server_wide`), what applies once it is cleared, and, for a server
 * administrator, the redaction limit an override also replaces. Clearing it puts them back on
 * the server's own limits.
 */
export function RateLimitSection({
  userId,
  admin = false,
  appserviceId,
}: {
  userId: string;
  /** Whether they are a server administrator: their redactions have their own limit. */
  admin?: boolean;
  /** The bridge whose namespace holds the account, if any. */
  appserviceId?: string | null;
}) {
  const { data, isLoading, isError, error, refetch } = useUserRateLimit(userId);
  const override = hasRateLimitOverride(data) ? data : undefined;
  const server = data?.server_wide;
  const canWrite = hasScope("admin:write");
  const context = rateLimitContext(server, Boolean(override), admin);

  return (
    <div>
      <h3 className="text-sm font-medium text-text">Message rate limit</h3>
      {isError ? (
        <QueryProblemState
          error={error}
          resource="this user's rate limit"
          onRetry={() => refetch()}
          compact
        />
      ) : isLoading ? (
        <p className="mt-1 text-sm text-text-muted">Loading…</p>
      ) : (
        <>
          <p className="mt-1 text-sm text-text" data-testid="rate-limit-current">
            {override
              ? `Override: ${describeRateLimit(override)}`
              : "No override: the server's own limits apply."}
          </p>
          <div className="mt-1 flex flex-col gap-1 text-sm text-text-muted">
            <p data-testid="rate-limit-server-wide">
              {context.serverWide}
              {context.override && ` ${context.override}`}{" "}
              <Link
                to="/configuration/$section"
                params={{ section: "rate_limits" }}
                hash={settingRowId(server && !server.enabled ? "enabled" : "message")}
                className="text-accent underline underline-offset-2"
              >
                Change it under Configuration, Rate limits
              </Link>
              .
            </p>
            {context.redactions && <p data-testid="rate-limit-redactions">{context.redactions}</p>}
            {appserviceId && (
              <p data-testid="rate-limit-bridge">
                This account belongs to the bridge{" "}
                <Link
                  to="/bridges/$bridgeId"
                  params={{ bridgeId: appserviceId }}
                  className="text-accent underline underline-offset-2"
                >
                  {appserviceId}
                </Link>
                . What the bridge sends for it is never limited when the bridge is registered with
                rate limiting off, override or not (its Edit dialog says which).
              </p>
            )}
          </div>
          {canWrite ? (
            <RateLimitForm
              // Remount when the saved override changes, so the fields show what is saved.
              key={JSON.stringify(override ? overrideFields(override) : null)}
              userId={userId}
              override={override}
              server={server}
            />
          ) : (
            <p className="mt-2 text-xs text-text-muted">Changing it needs admin:write.</p>
          )}
        </>
      )}
    </div>
  );
}

/** The override's own fields, without the server-wide limits the answer carries beside them. */
function overrideFields(o: RateLimitOverride): RateLimitOverride {
  return { messages_per_second: o.messages_per_second, burst_count: o.burst_count };
}

function RateLimitForm({
  userId,
  override,
  server,
}: {
  userId: string;
  override?: RateLimitOverride;
  server?: ServerRateLimits;
}) {
  const save = useSetUserRateLimit();
  const clear = useClearUserRateLimit();
  const [rate, setRate] = useState(
    override?.messages_per_second != null ? String(override.messages_per_second) : "",
  );
  const [burst, setBurst] = useState(String(override?.burst_count ?? DEFAULT_BURST_COUNT));
  const [errors, setErrors] = useState<FieldErrors>({});

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    const next: FieldErrors = {};
    const rateValue = Number(rate);
    const burstValue = Number(burst);
    if (rate.trim() === "" || !Number.isFinite(rateValue) || rateValue < 0) {
      next.rate = "A number of messages a second, 0 or more. 0 exempts them.";
    }
    if (!Number.isInteger(burstValue) || burstValue < 1) {
      next.burst = "A whole number, 1 or more.";
    }
    setErrors(next);
    if (next.rate || next.burst) return;
    try {
      await save.mutateAsync({
        userId,
        override: { messages_per_second: rateValue, burst_count: burstValue },
      });
      toast({ title: "Rate limit saved" });
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const first = err.problem.errors?.[0];
        if (first?.pointer === "/messages_per_second") return setErrors({ rate: first.detail });
        if (first?.pointer === "/burst_count") return setErrors({ burst: first.detail });
      }
      setErrors({ other: err });
    }
  }

  async function handleClear() {
    setErrors({});
    try {
      await clear.mutateAsync({ userId });
      toast({ title: "Rate limit override cleared" });
    } catch (err) {
      setErrors({ other: err });
    }
  }

  return (
    <form onSubmit={handleSubmit} className="mt-3 flex flex-col gap-3" noValidate>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field
          label="Messages per second"
          hint={
            server
              ? `0 exempts them from the limit. Server-wide: ${server.message.per_second.toLocaleString()}.`
              : "0 exempts them from the limit."
          }
          error={errors.rate}
        >
          {(fieldProps) => (
            <Input
              {...fieldProps}
              type="number"
              inputMode="decimal"
              min={0}
              step="any"
              value={rate}
              onChange={(e) => setRate(e.target.value)}
            />
          )}
        </Field>
        <Field
          label="Burst"
          hint={
            server
              ? `Messages they may send at once. Server-wide: ${server.message.burst_count.toLocaleString()}.`
              : "Messages they may send at once."
          }
          error={errors.burst}
        >
          {(fieldProps) => (
            <Input
              {...fieldProps}
              type="number"
              inputMode="numeric"
              min={1}
              step={1}
              value={burst}
              onChange={(e) => setBurst(e.target.value)}
            />
          )}
        </Field>
      </div>
      {errors.other != null && <MutationError error={errors.other} action="change the limit" />}
      <div className="flex flex-wrap gap-2">
        <Button type="submit" variant="secondary" size="sm" disabled={save.isPending}>
          {save.isPending ? "Saving…" : "Save limit"}
        </Button>
        {override && (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={clear.isPending}
            onClick={handleClear}
          >
            Clear override
          </Button>
        )}
      </div>
    </form>
  );
}
