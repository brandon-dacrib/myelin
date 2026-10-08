/**
 * What a user's rate-limit override stands in for, in words: the server-wide limits
 * (`ServerRateLimits`, from `GET /users/{user_id}/rate-limit`'s `server_wide`) beside the
 * override, so the user's page says what the override replaces and what applies once it is
 * cleared. The rules are the server's (`hs_room::moderation`): an override replaces
 * `rate_limits.message` for that user, and applies even when `rate_limits.enabled` is off; a
 * server administrator's redactions are under `rate_limits.admin_redaction` instead, unless they
 * have an override; a bucket at 0 a second limits nobody.
 */
import type { components } from "@/api/schema";

export type ServerRateLimits = components["schemas"]["ServerRateLimits"];
export type RateLimitBucket = components["schemas"]["RateLimitBucket"];

/** "0.5 messages a second, bursts of 25"; `noun` is singular. */
export function describeBucket(bucket: RateLimitBucket, noun = "message"): string {
  const rate = bucket.per_second;
  const per = rate === 1 ? `1 ${noun} a second` : `${rate.toLocaleString()} ${noun}s a second`;
  return `${per}, bursts of ${bucket.burst_count.toLocaleString()}`;
}

/** Whether the server-wide message limit limits anybody at all. */
export function serverLimitOn(server: ServerRateLimits): boolean {
  return server.enabled && server.message.per_second > 0;
}

/**
 * The sentences beside the override: what the server applies without one (`serverWide`), what
 * the override does to it (`override`, only when one is set), and, for a server administrator,
 * the redaction limit (`redactions`). `null` for a sentence that does not apply.
 */
export interface RateLimitContext {
  serverWide: string;
  override: string | null;
  redactions: string | null;
}

export function rateLimitContext(
  server: ServerRateLimits | undefined,
  hasOverride: boolean,
  admin: boolean,
): RateLimitContext {
  if (!server) {
    return {
      serverWide: "The server-wide limit could not be read here.",
      override: hasOverride
        ? "This override replaces it; clearing the override puts them back on it."
        : null,
      redactions: null,
    };
  }
  let serverWide: string;
  if (!server.enabled) {
    serverWide =
      "Server-wide, rate limits are switched off, so nobody without an override is limited.";
  } else if (server.message.per_second <= 0) {
    serverWide =
      "Server-wide, the message limit is off (0 a second), so nobody without an override is limited.";
  } else {
    serverWide = `Server-wide limit: ${describeBucket(server.message)}.`;
  }
  const override = !hasOverride
    ? null
    : serverLimitOn(server)
      ? "This override replaces it for them; clearing the override puts them back on it."
      : "This override still applies to them; clearing it leaves them unlimited.";
  let redactions: string | null = null;
  if (admin) {
    const bucket = server.admin_redaction;
    const limit =
      server.enabled && bucket.per_second > 0
        ? `the administrator redaction limit instead: ${describeBucket(bucket, "redaction")}`
        : "no limit (the administrator redaction limit is off)";
    redactions = hasOverride
      ? `As a server administrator their redactions would otherwise have ${limit}; the override covers their redactions too.`
      : `As a server administrator, their redactions have ${limit}.`;
  }
  return { serverWide, override, redactions };
}
