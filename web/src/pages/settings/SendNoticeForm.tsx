import { useState, type FormEvent, type ReactNode } from "react";
import { CircleCheck } from "lucide-react";
import { useServerInfo } from "@/api/dashboard";
import { useSendServerNotice, type ServerNoticeView } from "@/api/server-notices";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Field, Textarea } from "@/components/ui/input/Input";
import { CopyableId } from "@/components/CopyableId";
import { RecipientsEditor } from "./RecipientsEditor";

type FieldName = "recipients" | "message";

/**
 * Sends a plain-text server notice (`POST /server-notices`) and then shows what was sent where:
 * each recipient's server-notices room and the event in it.
 *
 * Used twice: on Settings, Server notices with a recipients editor, and from a user's page with
 * that one user fixed as the recipient (`fixedRecipient`), inside a dialog that `onDone` closes.
 */
export function SendNoticeForm({
  fixedRecipient,
  onDone,
  doneLabel = "Send another",
}: {
  fixedRecipient?: string;
  /** Called by the button on the confirmation; without it the form resets for another notice. */
  onDone?: () => void;
  doneLabel?: string;
}) {
  const send = useSendServerNotice();
  const { data: server } = useServerInfo();
  const [recipients, setRecipients] = useState<string[]>(fixedRecipient ? [fixedRecipient] : []);
  const [message, setMessage] = useState("");
  const [error, setError] = useState<{ message: string; field: FieldName | null } | null>(null);
  const [sent, setSent] = useState<ServerNoticeView | null>(null);

  function reset() {
    setRecipients(fixedRecipient ? [fixedRecipient] : []);
    setMessage("");
    setError(null);
    setSent(null);
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    if (recipients.length === 0) {
      setError({ message: "Add at least one recipient.", field: "recipients" });
      return;
    }
    if (!message.trim()) {
      setError({ message: "Write the message to send.", field: "message" });
      return;
    }
    try {
      setSent(await send.mutateAsync({ recipients, body: message.trim() }));
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const { problem } = err;
        const first = problem.errors?.[0];
        const pointer = first?.pointer ?? "";
        const field: FieldName | null = pointer.startsWith("/recipients")
          ? "recipients"
          : pointer.startsWith("/content")
            ? "message"
            : problem.status === 404
              ? "recipients"
              : null;
        const text =
          problem.status === 403
            ? "Sending notices needs the moderation:write scope."
            : (first?.detail ?? problem.detail ?? problem.title ?? "The server refused.");
        setError({ message: text, field });
        return;
      }
      setError({ message: "Couldn’t reach the server.", field: null });
    }
  }

  if (sent) {
    return (
      <SentConfirmation
        notice={sent}
        action={
          <Button onClick={onDone ?? reset} variant={onDone ? "primary" : "secondary"}>
            {onDone ? "Done" : doneLabel}
          </Button>
        }
      />
    );
  }

  const fieldError = (field: FieldName) => (error?.field === field ? error.message : undefined);

  return (
    <form className="flex flex-col gap-4" onSubmit={handleSubmit} noValidate>
      {fixedRecipient ? (
        <div className="flex flex-col gap-1.5">
          <span className="text-sm font-medium text-text">Recipient</span>
          <p className="font-identifier text-base text-text">{fixedRecipient}</p>
          {fieldError("recipients") && (
            <p role="alert" className="text-xs text-danger">
              {fieldError("recipients")}
            </p>
          )}
        </div>
      ) : (
        <RecipientsEditor
          value={recipients}
          onChange={(next) => {
            setRecipients(next);
            if (error?.field === "recipients") setError(null);
          }}
          serverName={server?.name}
          error={fieldError("recipients")}
        />
      )}
      <Field
        label="Message"
        hint="Plain text. It arrives in their server-notices room, from the server itself."
        error={fieldError("message")}
        required
      >
        {(fieldProps) => (
          <Textarea
            {...fieldProps}
            rows={5}
            value={message}
            onChange={(e) => setMessage(e.target.value)}
          />
        )}
      </Field>

      {error && error.field === null && (
        <p role="alert" className="text-sm text-danger">
          {error.message}
        </p>
      )}

      <div className="flex justify-end gap-2">
        {onDone && (
          <Button type="button" variant="ghost" onClick={onDone}>
            Cancel
          </Button>
        )}
        <Button type="submit" disabled={send.isPending}>
          {send.isPending
            ? "Sending…"
            : recipients.length > 1
              ? `Send to ${recipients.length} users`
              : "Send notice"}
        </Button>
      </div>
    </form>
  );
}

/** What a sent notice reached: one row per recipient, with their room and the event. */
function SentConfirmation({ notice, action }: { notice: ServerNoticeView; action: ReactNode }) {
  const count = notice.recipients.length;
  return (
    <div role="status" className="flex flex-col gap-3">
      <p className="flex items-center gap-2 text-sm font-medium text-text">
        <CircleCheck size={16} aria-hidden="true" className="text-success" />
        Notice sent to {count} {count === 1 ? "user" : "users"}.
      </p>
      <ul className="divide-y divide-border rounded-md border border-border text-sm">
        {notice.recipients.map((recipient, i) => (
          <li key={recipient} className="flex flex-col gap-1 px-3 py-2">
            <span className="font-identifier font-medium text-text">{recipient}</span>
            <span className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-text-muted">
              {notice.roomIds[i] && (
                <span className="inline-flex items-center gap-1">
                  Room <CopyableId value={notice.roomIds[i]} />
                </span>
              )}
              {notice.eventIds[i] && (
                <span className="inline-flex items-center gap-1">
                  Event <CopyableId value={notice.eventIds[i]} />
                </span>
              )}
            </span>
          </li>
        ))}
      </ul>
      <div className="flex justify-end">{action}</div>
    </div>
  );
}
