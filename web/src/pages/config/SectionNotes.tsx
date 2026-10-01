/**
 * What a section's settings add up to, where the settings one by one do not say it: how rate
 * limits are counted and what a client sees when it hits one. Shown above the form of the
 * section it belongs to (`ConfigSectionPage`).
 */
import { Gauge } from "lucide-react";

/** `rate_limits`: buckets, the per-replica count, and what a `429` looks like to a client. */
export function RateLimitsNote() {
  return (
    <section
      aria-labelledby="rate-limits-note-heading"
      className="mt-4 flex items-start gap-3 rounded-md border border-info-border bg-info-bg p-4"
    >
      <Gauge size={18} aria-hidden="true" className="mt-0.5 shrink-0 text-text-muted" />
      <div className="min-w-0 text-sm text-text-muted">
        <h2 id="rate-limits-note-heading" className="font-medium text-text">
          How rate limits work
        </h2>
        <p className="mt-1">
          Each limit is a bucket: <strong className="text-text">Burst count</strong> requests can be
          made back to back, and the bucket refills at{" "}
          <strong className="text-text">Per second</strong> (0.2 is one every five seconds).
          Messages and joins are counted per user, login and registration per client address (the
          first address a proxy in front of the server forwards), federation per remote server.
          Bridges registered without rate limiting are not counted.
        </p>
        <p className="mt-2">
          <strong className="text-text">What a client sees.</strong> A request over the limit is
          refused with HTTP 429 and the error{" "}
          <code className="font-identifier">M_LIMIT_EXCEEDED</code>, which says how long to wait (
          <code className="font-identifier">retry_after_ms</code>). Element and other Matrix clients
          wait that long and send again on their own, so a person sees a message take a moment to
          send rather than an error. A script that ignores the wait keeps being refused.
        </p>
        <p className="mt-2">
          <strong className="text-text">In a cluster, each replica counts on its own.</strong> A
          client whose requests are spread over three replicas can make up to three times the limit
          before every replica refuses it; set the limits for one replica. Changes apply on save on
          every replica (within ten seconds on the others), and what a client has left in its bucket
          is kept.
        </p>
      </div>
    </section>
  );
}
