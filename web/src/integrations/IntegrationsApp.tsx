import { useState, useSyncExternalStore, type FormEvent } from "react";
import { LogOut, Plus, Trash2, Webhook } from "lucide-react";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { Field, Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { Toaster } from "@/components/ui/toast/Toaster";
import { toast } from "@/components/ui/toast/toast-store";
import { CopyableId } from "@/components/CopyableId";
import { FileBlock, SetupText } from "@/pages/bridges/offering/AddWebhookDialog";
import { ChoiceCard } from "@/pages/bridges/offering-fields";
import { RelativeTime } from "@/components/RelativeTime";
import {
  IntegrationsSignInError,
  getUserSession,
  signInWithPassword,
  signOut,
  subscribeUserSession,
} from "./auth";
import {
  MatrixApiError,
  describeError,
  useCreateWebhook,
  useMyRooms,
  useMyWebhooks,
  useRemoveWebhook,
  useWebhookSources,
  type UserRoom,
  type UserWebhook,
  type WebhookCreated,
  type WebhookSource,
} from "./api";

/**
 * The self-service integrations page (decision 0057): anyone with an account on this server
 * signs in with it, sees the webhooks in their rooms, adds one (what will post, which room, a
 * name; the URL shown once with the sender's setup), and removes one. Plain and explained
 * inline: the person here is not an administrator and has not read the docs.
 */
export function IntegrationsApp() {
  const session = useSyncExternalStore(subscribeUserSession, getUserSession, getUserSession);
  return (
    <div className="min-h-screen bg-canvas text-text">
      <main className="mx-auto w-full max-w-3xl px-4 py-8 sm:px-6">
        {session ? <Signed userId={session.userId} /> : <SignIn />}
      </main>
      <Toaster />
    </div>
  );
}

function Brand() {
  return (
    <div className="flex items-center gap-3">
      <span
        aria-hidden="true"
        className="flex size-9 items-center justify-center rounded-md bg-accent text-accent-text-on"
      >
        <Webhook size={18} />
      </span>
      <div>
        <h1 className="text-lg font-semibold text-text">Integrations</h1>
        <p className="text-xs text-text-muted">Webhooks into your Matrix rooms</p>
      </div>
    </div>
  );
}

function SignIn() {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setPending(true);
    setError(null);
    try {
      await signInWithPassword(username, password);
    } catch (err) {
      setError(err instanceof IntegrationsSignInError ? err.message : "Sign-in failed. Try again.");
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="mx-auto max-w-md">
      <Brand />
      <p className="mt-6 text-sm text-text-muted">
        Get notifications from GitHub, Datadog, Linear, Notion, Gmail, Alertmanager or any script
        posted into a room of yours. Sign in with the Matrix account you use on this server; no
        administrator is needed.
      </p>
      <form className="mt-6 flex flex-col gap-3" onSubmit={submit}>
        <Field label="Username" required>
          {(f) => (
            <Input
              {...f}
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              autoComplete="username"
              placeholder="alice"
            />
          )}
        </Field>
        <Field label="Password" required>
          {(f) => (
            <Input
              {...f}
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="current-password"
            />
          )}
        </Field>
        {error && (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        )}
        <Button type="submit" disabled={pending} className="mt-1">
          {pending ? "Signing in..." : "Sign in"}
        </Button>
      </form>
      <p className="mt-6 text-xs text-text-faint">
        Your password goes to this server only, the same way your Matrix client sends it. An
        administrator manages the server itself at{" "}
        <a href="/admin/" className="text-accent underline hover:no-underline">
          /admin/
        </a>
        .
      </p>
    </div>
  );
}

function Signed({ userId }: { userId: string }) {
  const webhooks = useMyWebhooks(true);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<UserWebhook | null>(null);
  const unavailable =
    webhooks.error instanceof MatrixApiError && webhooks.error.errcode === "M_NOT_FOUND";

  return (
    <>
      <header className="flex flex-wrap items-center justify-between gap-3">
        <Brand />
        <div className="flex items-center gap-3">
          <span className="font-identifier text-xs text-text-muted" data-testid="signed-in-as">
            {userId}
          </span>
          <Button
            variant="secondary"
            size="sm"
            leadingIcon={<LogOut size={14} aria-hidden="true" />}
            onClick={() => void signOut()}
          >
            Sign out
          </Button>
        </div>
      </header>

      <section aria-labelledby="my-webhooks" className="mt-8">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h2 id="my-webhooks" className="text-md font-semibold text-text">
            My webhooks
          </h2>
          <Button
            size="sm"
            leadingIcon={<Plus size={14} aria-hidden="true" />}
            disabled={unavailable}
            onClick={() => setAdding(true)}
          >
            Add a webhook
          </Button>
        </div>
        <p className="mt-1 text-sm text-text-muted">
          A webhook is a URL that something outside (GitHub, Datadog, a script) posts to; what
          arrives is shaped for the sender and posted into the room by the bridge&apos;s bot. These
          are the webhooks in rooms you are in.
        </p>
        {webhooks.isLoading ? (
          <div className="mt-4">
            <SkeletonText lines={3} />
          </div>
        ) : unavailable ? (
          <div
            role="status"
            className="mt-4 rounded-md border border-border bg-surface-sunken p-4 text-sm"
          >
            <p className="font-medium text-text">Webhooks are not available on this server yet</p>
            <p className="mt-1 text-text-muted">{describeError(webhooks.error)}</p>
          </div>
        ) : webhooks.isError ? (
          <div role="alert" className="mt-4 rounded-md border border-danger/40 p-4 text-sm">
            <p className="font-medium text-text">Couldn&apos;t load your webhooks</p>
            <p className="mt-1 text-text-muted">{describeError(webhooks.error)}</p>
            <Button
              variant="secondary"
              size="sm"
              className="mt-3"
              onClick={() => void webhooks.refetch()}
            >
              Try again
            </Button>
          </div>
        ) : webhooks.data && webhooks.data.length === 0 ? (
          <EmptyState
            icon={<Webhook aria-hidden="true" />}
            title="No webhooks in your rooms yet"
            description="Add one: pick what will post, a room of yours or a new one, and a name. You get a URL to give the sender."
            action={<Button onClick={() => setAdding(true)}>Add a webhook</Button>}
          />
        ) : (
          <ul className="mt-4 flex flex-col gap-2" data-testid="webhook-list">
            {webhooks.data?.map((w) => (
              <li
                key={`${w.room_id}/${w.name}`}
                className="flex flex-wrap items-start justify-between gap-3 rounded-md border border-border bg-surface p-4"
              >
                <div className="min-w-0">
                  <p className="font-medium text-text">
                    <code className="font-identifier">{w.name}</code>
                    {w.source && (
                      <span className="ml-2 text-xs text-text-muted">{sourceTitle(w.source)}</span>
                    )}
                  </p>
                  <p className="mt-1 text-sm text-text-muted [overflow-wrap:anywhere]">
                    In <strong className="text-text">{w.room_name ?? w.room_id}</strong>
                    {w.created_by && (
                      <>
                        {" "}
                        · made by <span className="font-identifier">{w.created_by}</span>
                      </>
                    )}
                    {w.created_at ? (
                      <>
                        {" "}
                        <RelativeTime at={new Date(w.created_at).toISOString()} />
                      </>
                    ) : null}
                  </p>
                </div>
                <Button
                  variant="secondary"
                  size="sm"
                  leadingIcon={<Trash2 size={14} aria-hidden="true" />}
                  onClick={() => setRemoving(w)}
                  aria-label={`Remove ${w.name}`}
                >
                  Remove
                </Button>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="mt-10 text-xs text-text-faint">
        <p>
          The URL is shown once, when the webhook is made: its last part is the secret, and anyone
          who has it can post into the room. If it leaks, remove the webhook and make a new one.
          Rooms that are encrypted cannot take a webhook, since the bridge posts in the clear.
        </p>
      </section>

      {adding && <AddWebhookDialog open={adding} onOpenChange={setAdding} />}
      {removing && (
        <RemoveWebhookDialog
          webhook={removing}
          onOpenChange={(open) => !open && setRemoving(null)}
        />
      )}
    </>
  );
}

const SOURCE_TITLES: Record<string, string> = {
  generic: "Plain JSON",
  alertmanager: "Alertmanager",
  datadog: "Datadog",
  "github-comments": "GitHub comments",
  gmail: "Gmail",
  linear: "Linear",
  notion: "Notion",
};

function sourceTitle(id: string): string {
  return SOURCE_TITLES[id] ?? id;
}

/**
 * Adds a webhook: the source, the room (one of mine where I may, or a new one), the name; then
 * the URL once with the sender's setup. The same shape as the admin page's dialog, with the
 * rooms read from the user API (each saying whether a webhook may be added there, and why not).
 */
function AddWebhookDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const sources = useWebhookSources(open);
  const rooms = useMyRooms(open);
  const create = useCreateWebhook();
  const [source, setSource] = useState("generic");
  const [roomMode, setRoomMode] = useState<"existing" | "new">("existing");
  const [roomId, setRoomId] = useState("");
  const [roomName, setRoomName] = useState("");
  const [roomTopic, setRoomTopic] = useState("");
  const [name, setName] = useState("");
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [made, setMade] = useState<WebhookCreated | null>(null);

  const eligible: UserRoom[] = (rooms.data ?? []).filter((r) => r.may_add);
  const ineligible: UserRoom[] = (rooms.data ?? []).filter((r) => !r.may_add);
  const chosen = sources.data?.find((s) => s.id === source);

  function handleOpenChange(next: boolean) {
    if (!next) {
      setErrors({});
      setMade(null);
      setName("");
      setRoomName("");
      setRoomTopic("");
    }
    onOpenChange(next);
  }

  function validate(): Record<string, string> {
    const found: Record<string, string> = {};
    const trimmed = name.trim();
    if (trimmed.length < 3 || trimmed.length > 64) found["/name"] = "3 to 64 characters.";
    if (roomMode === "existing" && !roomId) found["/room/id"] = "Pick a room.";
    if (roomMode === "new" && !roomName.trim())
      found["/room/create/name"] = "The room needs a name.";
    return found;
  }

  async function submit() {
    const found = validate();
    setErrors(found);
    if (Object.keys(found).length) return;
    try {
      const created = await create.mutateAsync({
        source,
        name: name.trim(),
        room:
          roomMode === "existing"
            ? { id: roomId }
            : { create: { name: roomName.trim(), topic: roomTopic.trim() || undefined } },
      });
      setMade(created);
      toast({ title: `Webhook ${created.name} is connected` });
    } catch (err) {
      setErrors({ "": describeError(err) });
    }
  }

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
        description="What arrives at a new URL is posted into the room, shaped for what sends it."
        footer={
          <>
            <Button variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button disabled={create.isPending || !sources.data || !rooms.data} onClick={submit}>
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
            <p className="mb-2 text-sm font-medium text-text" id="user-webhook-source-label">
              What will post to it
            </p>
            {sources.isError ? (
              <p role="alert" className="text-sm text-danger">
                {describeError(sources.error)}
              </p>
            ) : sources.isLoading || !sources.data ? (
              <SkeletonText lines={3} />
            ) : (
              <div
                role="radiogroup"
                aria-labelledby="user-webhook-source-label"
                className="grid grid-cols-1 gap-2 sm:grid-cols-2"
              >
                {sources.data.map((s: WebhookSource) => (
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
            <p className="mb-2 text-sm font-medium text-text" id="user-webhook-room-label">
              Which room
            </p>
            <div
              role="radiogroup"
              aria-labelledby="user-webhook-room-label"
              className="grid grid-cols-1 gap-2 sm:grid-cols-2"
            >
              <ChoiceCard
                selected={roomMode === "existing"}
                onSelect={() => setRoomMode("existing")}
                title="One of my rooms"
                description={
                  rooms.isLoading
                    ? "Looking at your rooms..."
                    : eligible.length
                      ? `${eligible.length} of your ${rooms.data?.length ?? 0} rooms can take one. The bridge's bot is invited to it for you.`
                      : "None of your rooms can take one yet (you need to be able to change the room's settings). Make a new room instead."
                }
                disabled={!rooms.isLoading && !eligible.length}
              />
              <ChoiceCard
                selected={roomMode === "new"}
                onSelect={() => setRoomMode("new")}
                title="A new room"
                description="Private, made for you: you are its admin, and the bridge's bot is in it."
              />
            </div>
            {rooms.isError && (
              <p role="alert" className="mt-2 text-sm text-danger">
                {describeError(rooms.error)}
              </p>
            )}
            {roomMode === "existing" ? (
              <div className="mt-3">
                <Field label="Room" error={errors["/room/id"]}>
                  {(f) => (
                    <Select
                      {...f}
                      value={roomId}
                      onValueChange={setRoomId}
                      options={eligible.map((r) => ({
                        value: r.room_id,
                        label: r.name ? r.name : r.room_id,
                      }))}
                      placeholder="Pick a room"
                      disabled={!eligible.length}
                    />
                  )}
                </Field>
                {ineligible.length > 0 && (
                  <details className="mt-2 text-xs text-text-muted">
                    <summary className="cursor-pointer">
                      {ineligible.length} of your rooms cannot take a webhook
                    </summary>
                    <ul className="mt-1 list-disc space-y-1 pl-5">
                      {ineligible.map((r) => (
                        <li key={r.room_id}>
                          <span className="text-text">{r.name ?? r.room_id}</span>: {r.reason}
                        </li>
                      ))}
                    </ul>
                  </details>
                )}
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
              </div>
            )}
          </div>

          <Field
            label="Name"
            hint="3 to 64 characters; what the room calls it."
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

function RemoveWebhookDialog({
  webhook,
  onOpenChange,
}: {
  webhook: UserWebhook;
  onOpenChange: (open: boolean) => void;
}) {
  const remove = useRemoveWebhook();
  const [error, setError] = useState<string | null>(null);
  async function confirm() {
    setError(null);
    try {
      await remove.mutateAsync({ roomId: webhook.room_id, name: webhook.name });
      toast({ title: `Webhook ${webhook.name} removed` });
      onOpenChange(false);
    } catch (err) {
      setError(describeError(err));
    }
  }
  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent
        title={`Remove ${webhook.name}?`}
        description={`Its URL stops working and nothing more from it lands in ${webhook.room_name ?? "the room"}. The room and its messages stay.`}
        footer={
          <>
            <Button variant="secondary" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button variant="danger" disabled={remove.isPending} onClick={() => void confirm()}>
              {remove.isPending ? "Removing..." : "Remove"}
            </Button>
          </>
        }
      >
        {error && (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        )}
      </DialogContent>
    </Dialog>
  );
}
