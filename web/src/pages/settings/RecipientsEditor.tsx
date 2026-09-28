import { useId, useState, type KeyboardEvent } from "react";
import { Plus, X } from "lucide-react";
import { useUserSuggestions } from "@/api/users";
import { Button } from "@/components/ui/button/Button";
import { Input } from "@/components/ui/input/Input";
import { parseRecipient, splitRecipients } from "@/lib/server-notices";
import { cn } from "@/lib/cn";

/**
 * A list editor for server-notice recipients: each one a chip with a remove button, added by
 * typing a user ID (or just a username, completed with this server's name) and pressing Enter
 * or Add, by pasting several at once, or by choosing one of the matching users suggested as
 * you type. Every ID is checked to be a local user ID before it becomes a chip.
 */
export function RecipientsEditor({
  value,
  onChange,
  serverName,
  error,
}: {
  value: string[];
  onChange: (recipients: string[]) => void;
  serverName?: string;
  /** A refusal from the server about the recipients as a whole. */
  error?: string;
}) {
  const inputId = useId();
  const hintId = useId();
  const errorId = useId();
  const suggestionsId = useId();
  const [draft, setDraft] = useState("");
  const [draftError, setDraftError] = useState<string | null>(null);
  const suggestions = useUserSuggestions(draft);
  const offered = (suggestions.data ?? []).filter((u) => !value.includes(u.user_id));
  const shownError = draftError ?? error;

  function add(entries: string[]) {
    const next = [...value];
    for (const entry of entries) {
      const parsed = parseRecipient(entry, serverName);
      if ("error" in parsed) {
        setDraftError(parsed.error);
        onChange(next);
        return false;
      }
      if (!next.includes(parsed.userId)) next.push(parsed.userId);
    }
    onChange(next);
    setDraftError(null);
    return true;
  }

  function commitDraft() {
    const entries = splitRecipients(draft);
    if (entries.length === 0) {
      setDraftError("Type a user ID.");
      return;
    }
    if (add(entries)) setDraft("");
  }

  function handleKeyDown(e: KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Enter") {
      e.preventDefault();
      commitDraft();
    } else if (e.key === "Backspace" && draft === "" && value.length > 0) {
      onChange(value.slice(0, -1));
    }
  }

  return (
    <div className="flex flex-col gap-1.5">
      <label htmlFor={inputId} className="text-sm font-medium text-text">
        Recipients
        <span aria-hidden="true" className="text-danger">
          {" "}
          *
        </span>
      </label>
      {value.length > 0 && (
        <ul aria-label="Chosen recipients" className="flex flex-wrap gap-1.5">
          {value.map((userId) => (
            <li
              key={userId}
              className="inline-flex items-center gap-1 rounded-full border border-border-strong bg-surface-sunken py-0.5 pl-2.5 pr-1 text-sm"
            >
              <span className="font-identifier text-text">{userId}</span>
              <button
                type="button"
                aria-label={`Remove ${userId}`}
                onClick={() => onChange(value.filter((r) => r !== userId))}
                className="rounded-full p-0.5 text-text-muted hover:bg-surface hover:text-text focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
              >
                <X size={12} aria-hidden="true" />
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="flex gap-2">
        <Input
          id={inputId}
          autoComplete="off"
          autoCapitalize="none"
          spellCheck={false}
          placeholder={serverName ? `@name:${serverName}, or search by name` : "@name:server"}
          aria-describedby={cn(shownError ? errorId : hintId, offered.length > 0 && suggestionsId)}
          aria-invalid={Boolean(shownError) || undefined}
          value={draft}
          onChange={(e) => {
            setDraft(e.target.value);
            setDraftError(null);
          }}
          onKeyDown={handleKeyDown}
        />
        <Button
          type="button"
          variant="secondary"
          leadingIcon={<Plus size={14} aria-hidden="true" />}
          onClick={commitDraft}
        >
          Add
        </Button>
      </div>
      {shownError ? (
        <p id={errorId} role="alert" className="text-xs text-danger">
          {shownError}
        </p>
      ) : (
        <p id={hintId} className="text-xs text-text-muted">
          Users on this server. Press Enter to add each one; paste several separated by commas.
        </p>
      )}
      {offered.length > 0 && (
        <div id={suggestionsId}>
          <p className="text-xs text-text-muted">Matching users</p>
          <ul className="mt-1 flex flex-col rounded-sm border border-border bg-surface">
            {offered.map((u) => (
              <li key={u.user_id}>
                <button
                  type="button"
                  aria-label={`Add ${u.user_id}`}
                  onClick={() => {
                    add([u.user_id]);
                    setDraft("");
                  }}
                  className="flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left text-sm hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
                >
                  <span className="font-identifier text-text">{u.user_id}</span>
                  {u.display_name && <span className="text-text-muted">{u.display_name}</span>}
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
