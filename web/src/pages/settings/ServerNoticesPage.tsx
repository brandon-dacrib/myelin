import { useNavigate, useSearch } from "@tanstack/react-router";
import { Megaphone } from "lucide-react";
import { useServerNotices, type ServerNoticeView } from "@/api/server-notices";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { hasScope } from "@/lib/auth";
import { SettingsTabs } from "./SettingsTabs";
import { SendNoticeForm } from "./SendNoticeForm";

/** How many recipients a history row names before "and N more". */
const RECIPIENTS_SHOWN = 3;

function recipientsSummary(recipients: string[]): string {
  const shown = recipients.slice(0, RECIPIENTS_SHOWN).join(", ");
  const more = recipients.length - RECIPIENTS_SHOWN;
  return more > 0 ? `${shown} and ${more} more` : shown;
}

/**
 * `/settings/server-notices`: send a message from the server to users on it, and see what was
 * sent before. A notice reaches each recipient in their own server-notices room.
 */
export function ServerNoticesPage() {
  const search = useSearch({ from: "/settings/server-notices" });
  const navigate = useNavigate({ from: "/settings/server-notices" });
  const canRead = hasScope("moderation:read");
  const canSend = hasScope("moderation:write");
  const { data, isLoading, isError, error, refetch } = useServerNotices(search.cursor, canRead);

  const columns: Column<ServerNoticeView>[] = [
    {
      key: "sent_at",
      header: "Sent",
      priority: 1,
      render: (n) => <RelativeTime at={n.sentAt} />,
    },
    {
      key: "recipients",
      header: "Recipients",
      priority: 1,
      render: (n) => (
        <span className="font-identifier text-sm" title={n.recipients.join("\n")}>
          {recipientsSummary(n.recipients)}
        </span>
      ),
      renderCompact: (n) => recipientsSummary(n.recipients),
    },
    {
      key: "message",
      header: "Message",
      priority: 1,
      render: (n) =>
        n.body !== null ? (
          <span className="line-clamp-2 max-w-xl text-sm text-text">{n.body}</span>
        ) : (
          <span className="text-sm text-text-muted">{n.type} without a text body</span>
        ),
      renderCompact: (n) => n.body ?? n.type,
    },
    {
      key: "sender",
      header: "Sender",
      priority: 3,
      render: (n) => <span className="font-identifier text-sm">{n.sender}</span>,
    },
  ];

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Settings</h1>
      <SettingsTabs current="server-notices" />

      <section aria-labelledby="send-notice-heading" className="mt-6 max-w-2xl">
        <h2 id="send-notice-heading" className="text-md font-medium text-text">
          Send a server notice
        </h2>
        <p className="mt-1 text-sm text-text-muted">
          A message from the server itself: planned downtime, a warning about an account, a change
          of rules. Each recipient gets it in their own server-notices room.
        </p>
        <div className="mt-4 rounded-md border border-border bg-surface p-4">
          {canSend ? <SendNoticeForm /> : <ForbiddenState scope="moderation:write" compact />}
        </div>
      </section>

      <section aria-labelledby="notice-history-heading" className="mt-8">
        <h2 id="notice-history-heading" className="text-md font-medium text-text">
          Sent notices
        </h2>
        {!canRead ? (
          <div className="mt-3">
            <ForbiddenState scope="moderation:read" compact />
          </div>
        ) : isError ? (
          <div className="mt-3">
            <QueryProblemState
              error={error}
              resource="sent notices"
              scope="moderation:read"
              onRetry={() => refetch()}
            />
          </div>
        ) : (
          <div className="mt-3">
            <DataTable
              caption="Sent server notices"
              columns={columns}
              rows={data?.items ?? []}
              getRowId={(n) => n.id}
              loading={isLoading}
              empty={
                <EmptyState
                  icon={<Megaphone aria-hidden="true" />}
                  title="No notices sent yet"
                  description="Notices you send appear here, newest first."
                />
              }
              pagination={
                data?.nextCursor || search.cursor
                  ? {
                      hasPrevious: Boolean(search.cursor),
                      hasNext: Boolean(data?.nextCursor),
                      onPrevious: () => navigate({ search: {} }),
                      onNext: () =>
                        data?.nextCursor && navigate({ search: { cursor: data.nextCursor } }),
                    }
                  : undefined
              }
            />
          </div>
        )}
      </section>
    </div>
  );
}
