/**
 * Catch-up, on the Federation pages: a destination that was down longer than its outbound
 * queue holds stops being queued for, and is caught up when it answers again
 * (`Destination.catch_up_since`, `hs_federation::sender`). The badge says so in a list; the
 * notice explains it on the destination's own page, with the queue limit this server runs with
 * (`federation.max_queued_pdus_per_destination`) and a link to change it.
 */
import { Link } from "@tanstack/react-router";
import { History } from "lucide-react";
import { useFederationQueueLimit } from "@/api/federation";
import { RelativeTime } from "@/components/RelativeTime";
import { Badge } from "@/components/ui/badge/Badge";
import {
  DEFAULT_MAX_QUEUED_PDUS,
  MAX_QUEUED_PDUS_SETTING,
  catchUpExplanation,
} from "@/lib/federation";
import { settingRowId } from "@/lib/config-model";

/** "Catching up since 3 h ago", for a list row. Nothing when the destination is not. */
export function CatchUpBadge({ since }: { since: string | null | undefined }) {
  if (!since) return null;
  return (
    <Badge status="info" hideIcon>
      <History size={12} aria-hidden="true" />
      <span>
        Catching up since <RelativeTime at={since} />
      </span>
    </Badge>
  );
}

/** The destination page's explanation of catch-up, shown while the destination is in it. */
export function CatchUpNotice({ since }: { since: string }) {
  const { limit } = useFederationQueueLimit();
  return (
    <section
      aria-labelledby="catch-up-heading"
      className="mt-6 flex items-start gap-3 rounded-md border border-info-border bg-info-bg p-4"
    >
      <History size={18} aria-hidden="true" className="mt-0.5 shrink-0 text-text-muted" />
      <div className="min-w-0">
        <h2 id="catch-up-heading" className="text-sm font-medium text-text">
          Catching up since <RelativeTime at={since} />
        </h2>
        <p className="mt-1 text-sm text-text-muted">{catchUpExplanation(limit)}</p>
        <p className="mt-2 text-sm text-text-muted">
          The queue limit is the setting{" "}
          <Link
            to="/configuration/$section"
            params={{ section: "federation" }}
            hash={settingRowId(MAX_QUEUED_PDUS_SETTING)}
            className="text-accent hover:underline"
          >
            Max queued PDUs per destination
          </Link>{" "}
          (default {DEFAULT_MAX_QUEUED_PDUS.toLocaleString("en-US")}). A higher limit keeps more
          events for a server that is down, at the cost of this server&apos;s database; it takes
          effect at the next restart.
        </p>
      </div>
    </section>
  );
}
