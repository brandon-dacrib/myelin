import { useState, type ReactNode } from "react";
import { Check, Copy } from "lucide-react";
import {
  useBridgeWebhookSources,
  useCreateBridgeConnection,
  type BridgeBotRoom,
  type BridgeConnectionCreated,
  type BridgeWebhookSource,
} from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { CopyableId } from "@/components/CopyableId";
import { QueryProblemState } from "@/components/QueryProblemState";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input, Textarea } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { toast } from "@/components/ui/toast/toast-store";
import { parseUserList } from "@/lib/bridge-offerings";
import { ChoiceCard } from "../offering-fields";

/**
 * Adds a webhook to a shared integrations bridge (Hookshot): pick what will post (a source the
 * server has a transformation for), the room (one the bot is in, or a new one with people
 * invited), and a name. The server makes the connection as the bot and answers with the URL
 * once; this shows it with a copy button and the sender's setup, and says it will not be
 * shown again (the secret is in the bridge's account data, which the API never reads back).
 */
export function AddWebhookDialog({
  type,
  bridgeName,
  rooms,
  open,
  onOpenChange,
}: {
  type: string;
  bridgeName: string;
  rooms: BridgeBotRoom[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const sources = useBridgeWebhookSources(type, open);
  const create = useCreateBridgeConnection();
  const [source, setSource] = useState<string>("generic");
  const [roomMode, setRoomMode] = useState<"existing" | "new">(rooms.length ? "existing" : "new");
  const [roomId, setRoomId] = useState<string>(rooms[0]?.room_id ?? "");
  const [roomName, setRoomName] = useState("");
  const [roomTopic, setRoomTopic] = useState("");
  const [invites, setInvites] = useState("");
  const [name, setName] = useState("");
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [made, setMade] = useState<BridgeConnectionCreated | null>(null);

  function handleOpenChange(next: boolean) {
    if (!next) {
      setErrors({});
      setMade(null);
      setName("");
      setRoomName("");
      setRoomTopic("");
      setInvites("");
    }
    onOpenChange(next);
  }

  function validate(): Record<string, string> {
    const found: Record<string, string> = {};
    const trimmed = name.trim();
    if (trimmed.length < 3 || trimmed.length > 64) {
      found["/name"] = "3 to 64 characters.";
    }
    if (roomMode === "existing" && !roomId) {
      found["/room/id"] = "Pick a room the bot is in.";
    }
    if (roomMode === "new" && !roomName.trim()) {
      found["/room/create/name"] = "The room needs a name.";
    }
    if (roomMode === "new") {
      const bad = parseUserList(invites).find((u) => !/^@[^:]+:.+$/.test(u));
      if (bad) found["/room/create/invite"] = `${bad} is not a user id (@user:server).`;
    }
    return found;
  }

  async function submit() {
    const found = validate();
    setErrors(found);
    if (Object.keys(found).length) return;
    try {
      const created = await create.mutateAsync({
        type,
        body: {
          source,
          name: name.trim(),
          room:
            roomMode === "existing"
              ? { id: roomId }
              : {
                  create: {
                    name: roomName.trim(),
                    topic: roomTopic.trim() || undefined,
                    invite: parseUserList(invites),
                  },
                },
        },
      });
      setMade(created);
      toast({ title: `Webhook ${created.name} is connected` });
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const first = err.problem.errors?.[0];
        if (first?.pointer) {
          setErrors({ [first.pointer]: first.detail ?? err.problem.detail ?? "Refused." });
        } else {
          setErrors({ "": err.problem.detail ?? err.problem.title ?? "The server refused." });
        }
      } else {
        setErrors({ "": "Couldn’t reach the server." });
      }
    }
  }

  const chosen = sources.data?.find((s) => s.id === source);

  if (made) {
    return (
      <Dialog open={open} onOpenChange={handleOpenChange}>
        <DialogContent
          size="form"
          title={`Webhook ${made.name} is connected`}
          description={
            <>
              Copy the URL now: <strong>it will not be shown again.</strong> Its last part is the
              secret; anyone who has it can post into the room.
            </>
          }
          footer={<Button onClick={() => handleOpenChange(false)}>Done</Button>}
        >
          <div
            className="rounded-md border border-border bg-surface-sunken p-3 [overflow-wrap:anywhere]"
            data-testid="webhook-url"
          >
            <CopyableId value={made.url} label="webhook URL" />
          </div>
          <h3 className="mt-4 text-sm font-medium text-text">
            On the sender&apos;s side ({chosen?.title ?? made.source})
          </h3>
          <SetupText text={made.setup} />
          {Object.entries(made.files).map(([file, text]) => (
            <FileBlock key={file} name={file} text={text} />
          ))}
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title="Add a webhook"
        description={`${bridgeName} will post what arrives at a new URL into the room, shaped for what sends it.`}
        footer={
          <>
            <Button variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button disabled={create.isPending || !sources.data} onClick={submit}>
              {create.isPending ? "Connecting..." : "Connect"}
            </Button>
          </>
        }
      >
        <form
          className="space-y-5"
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
        >
          <div>
            <p className="mb-2 text-sm font-medium text-text" id="webhook-source-label">
              What will post to it
            </p>
            {sources.isError ? (
              <QueryProblemState
                error={sources.error}
                resource="webhook sources"
                scope="bridges:read"
                onRetry={() => sources.refetch()}
              />
            ) : sources.isLoading || !sources.data ? (
              <SkeletonText lines={3} />
            ) : (
              <div
                role="radiogroup"
                aria-labelledby="webhook-source-label"
                className="grid grid-cols-1 gap-2 sm:grid-cols-2"
              >
                {sources.data.map((s: BridgeWebhookSource) => (
                  <ChoiceCard
                    key={s.id}
                    selected={source === s.id}
                    onSelect={() => setSource(s.id)}
                    title={s.title}
                    description={s.description}
                  />
                ))}
              </div>
            )}
          </div>

          <div>
            <p className="mb-2 text-sm font-medium text-text" id="webhook-room-label">
              Which room
            </p>
            <div
              role="radiogroup"
              aria-labelledby="webhook-room-label"
              className="grid grid-cols-1 gap-2 sm:grid-cols-2"
            >
              <ChoiceCard
                selected={roomMode === "existing"}
                onSelect={() => setRoomMode("existing")}
                title="A room the bot is in"
                description={
                  rooms.length
                    ? `${rooms.length} ${rooms.length === 1 ? "room" : "rooms"}; the bot must be allowed to send state events there.`
                    : "None yet: invite the bot to a room, or make one here."
                }
                disabled={!rooms.length}
              />
              <ChoiceCard
                selected={roomMode === "new"}
                onSelect={() => setRoomMode("new")}
                title="A new room"
                description="Made by the bot, private, with the people you name invited."
              />
            </div>
            {roomMode === "existing" ? (
              <div className="mt-3">
                <Field label="Room" error={errors["/room/id"]}>
                  {(f) => (
                    <Select
                      {...f}
                      value={roomId}
                      onValueChange={setRoomId}
                      options={rooms.map((r) => ({
                        value: r.room_id,
                        label: r.room_name ? `${r.room_name} (${r.room_id})` : r.room_id,
                      }))}
                      placeholder="Pick a room"
                    />
                  )}
                </Field>
              </div>
            ) : (
              <div className="mt-3 space-y-3">
                <Field label="Room name" error={errors["/room/create/name"]} required>
                  {(f) => (
                    <Input
                      {...f}
                      value={roomName}
                      onChange={(e) => setRoomName(e.target.value)}
                      placeholder="Alerts"
                    />
                  )}
                </Field>
                <Field label="Topic">
                  {(f) => (
                    <Input
                      {...f}
                      value={roomTopic}
                      onChange={(e) => setRoomTopic(e.target.value)}
                    />
                  )}
                </Field>
                <Field
                  label="Invite"
                  hint="User ids, one per line or separated by commas."
                  error={errors["/room/create/invite"]}
                >
                  {(f) => (
                    <Textarea
                      {...f}
                      value={invites}
                      onChange={(e) => setInvites(e.target.value)}
                      placeholder="@alice:example.org"
                    />
                  )}
                </Field>
              </div>
            )}
          </div>

          <Field
            label="Name"
            hint="3 to 64 characters; what the room and the list call it."
            error={errors["/name"]}
            required
          >
            {(f) => (
              <Input
                {...f}
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder={chosen && chosen.id !== "generic" ? chosen.id : "deploys"}
              />
            )}
          </Field>
          {errors[""] && (
            <p role="alert" className="text-sm text-danger">
              {errors[""]}
            </p>
          )}
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** A file the sender runs (`Code.gs`), with a copy button. */
function FileBlock({ name, text }: { name: string; text: string }) {
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable: the text stays selectable */
    }
  }
  return (
    <div className="mt-4">
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium text-text">
          <code className="font-identifier">{name}</code>
        </h3>
        <Button
          variant="secondary"
          size="sm"
          onClick={copy}
          leadingIcon={
            copied ? <Check size={14} aria-hidden="true" /> : <Copy size={14} aria-hidden="true" />
          }
        >
          {copied ? "Copied" : `Copy ${name}`}
        </Button>
      </div>
      <pre className="mt-2 max-h-64 overflow-auto rounded-md border border-border bg-surface-sunken p-3 text-xs text-text">
        {text}
      </pre>
    </div>
  );
}

/**
 * The sender's setup, which the server writes in Markdown (a heading, paragraphs, lists, code
 * fences, inline code, bold, links): rendered here with a reader for just that, built as React
 * nodes, never as HTML.
 */
export function SetupText({ text }: { text: string }) {
  const blocks: ReactNode[] = [];
  const lines = text.split("\n");
  let i = 0;
  let key = 0;
  while (i < lines.length) {
    const line = lines[i]!;
    if (line.startsWith("```")) {
      const code: string[] = [];
      i += 1;
      while (i < lines.length && !lines[i]!.startsWith("```")) {
        code.push(lines[i]!);
        i += 1;
      }
      i += 1;
      blocks.push(
        <pre
          key={key++}
          className="overflow-auto rounded-md border border-border bg-surface-sunken p-3 text-xs text-text"
        >
          {code.join("\n")}
        </pre>,
      );
      continue;
    }
    if (/^#{1,3} /.test(line)) {
      // The source's own title: the dialog already names it.
      i += 1;
      continue;
    }
    if (/^(- |\d+\. )/.test(line)) {
      const ordered = /^\d+\. /.test(line);
      const items: string[] = [];
      while (i < lines.length && /^(- |\d+\. |\s{2,}\S)/.test(lines[i]!)) {
        if (/^(- |\d+\. )/.test(lines[i]!)) {
          items.push(lines[i]!.replace(/^(- |\d+\. )/, ""));
        } else {
          items[items.length - 1] += " " + lines[i]!.trim();
        }
        i += 1;
      }
      const Tag = ordered ? "ol" : "ul";
      blocks.push(
        <Tag
          key={key++}
          className={ordered ? "list-decimal space-y-1 pl-5" : "list-disc space-y-1 pl-5"}
        >
          {items.map((item, n) => (
            <li key={n}>{inline(item)}</li>
          ))}
        </Tag>,
      );
      continue;
    }
    if (line.trim() === "") {
      i += 1;
      continue;
    }
    const para: string[] = [];
    while (
      i < lines.length &&
      lines[i]!.trim() !== "" &&
      !/^(```|#{1,3} |- |\d+\. )/.test(lines[i]!)
    ) {
      para.push(lines[i]!.trim());
      i += 1;
    }
    blocks.push(
      <p key={key++} className="[overflow-wrap:anywhere]">
        {inline(para.join(" "))}
      </p>,
    );
  }
  return (
    <div className="mt-2 space-y-3 text-sm text-text-muted" data-testid="webhook-setup">
      {blocks}
    </div>
  );
}

/** Inline code, bold and links; everything else is text. */
function inline(text: string): ReactNode[] {
  const out: ReactNode[] = [];
  const pattern =
    /(`[^`]+`|\*\*[^*]+\*\*|\[[^\]]+\]\((https?:\/\/[^)\s]+)\)|<(https?:\/\/[^>\s]+)>)/g;
  let last = 0;
  let key = 0;
  for (const match of text.matchAll(pattern)) {
    const at = match.index ?? 0;
    if (at > last) out.push(text.slice(last, at));
    const token = match[0];
    if (token.startsWith("`")) {
      out.push(
        <code key={key++} className="font-identifier text-text">
          {token.slice(1, -1)}
        </code>,
      );
    } else if (token.startsWith("**")) {
      out.push(
        <strong key={key++} className="text-text">
          {token.slice(2, -2)}
        </strong>,
      );
    } else if (token.startsWith("[")) {
      const label = token.slice(1, token.indexOf("]("));
      out.push(
        <a
          key={key++}
          href={match[2]}
          target="_blank"
          rel="noreferrer"
          className="text-accent underline hover:no-underline"
        >
          {label}
        </a>,
      );
    } else {
      out.push(
        <a
          key={key++}
          href={match[3]}
          target="_blank"
          rel="noreferrer"
          className="text-accent underline hover:no-underline"
        >
          {match[3]}
        </a>,
      );
    }
    last = at + token.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}
