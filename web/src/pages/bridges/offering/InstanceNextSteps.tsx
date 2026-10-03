import { useState, useSyncExternalStore } from "react";
import { Check, Copy, ExternalLink } from "lucide-react";
import {
  useAppserviceLogins,
  type BridgeInstance,
  type BridgeOffering,
  type BridgeType,
} from "@/api/bridges";
import { CopyableId } from "@/components/CopyableId";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { getSession, subscribeSession } from "@/lib/auth";
import { signInSteps } from "@/lib/bridge-catalogue";
import {
  firstCommand,
  instanceBotId,
  nextStepsMessage,
  nextStepsPhase,
  type NextStepsPhase,
} from "@/lib/bridge-next-steps";
import { instanceStateBadge, instanceStateLabel } from "@/lib/bridge-offerings";
import { withInlineCode } from "@/lib/inline-code";

/**
 * What to do next about one person's bridge, written for the operator who will relay it (RFC
 * 0017 sections 4.1 and 4.2). While the bridge is on its way it says what is happening and that
 * the steps come when it is ready; once ready and not signed in, it names the person's own bot,
 * says the bot has invited them to a chat, lists the catalogue's steps with that bot filled in,
 * and offers the whole thing as one message to paste. When the person is the operator, it says
 * so and tells them what to send. Asks the bridge whether they have signed in through the same
 * query as the table's "Signed in to" cell, so the answer is fetched once.
 */
export function InstanceNextSteps({
  instance,
  offering,
  type,
}: {
  instance: BridgeInstance;
  offering: BridgeOffering;
  type: BridgeType | undefined;
}) {
  const session = useSyncExternalStore(subscribeSession, getSession, getSession);
  const owner = instance.user_id ?? "";
  const isSelf = Boolean(owner) && owner === session?.operator.subject;
  const ready = instance.state === "ready" && Boolean(instance.appservice_id);
  const logins = useAppserviceLogins(
    ready ? (instance.appservice_id ?? undefined) : undefined,
    undefined,
  );
  const phase = nextStepsPhase(instance, offering.runtime, logins);
  return (
    <InstanceNextStepsBody
      instance={instance}
      offering={offering}
      type={type}
      phase={phase}
      isSelf={isSelf}
    />
  );
}

/** The words for a phase already decided; `InstanceNextSteps` decides it. */
export function InstanceNextStepsBody({
  instance,
  offering,
  type,
  phase,
  isSelf,
}: {
  instance: BridgeInstance;
  offering: BridgeOffering;
  type: BridgeType | undefined;
  phase: NextStepsPhase;
  isSelf: boolean;
}) {
  const name = offering.name ?? offering.type;
  const owner = instance.user_id ?? "";
  const bot = instanceBotId(instance, offering);
  const who = isSelf ? "you" : owner;
  const them = isSelf ? "you" : "them";
  const detail = instance.reason ?? instance.deployment?.message ?? null;

  switch (phase.kind) {
    case "setting-up":
      return (
        <div className="flex flex-col gap-2 text-sm text-text">
          <StateLine instance={instance} />
          <p>
            {isSelf ? `Setting up your ${name} bridge.` : `Setting up ${owner}'s ${name} bridge.`}{" "}
            This usually takes a minute or two; this page follows it.
          </p>
          {detail && <p className="text-text-muted">{detail}</p>}
          <p className="text-text-muted">
            When it is ready, its bot
            {bot && (
              <>
                , <CopyableId value={bot} />,
              </>
            )}{" "}
            invites {them} to a direct chat with the sign-in steps, and the steps appear here too.
            Nothing to {isSelf ? "do" : `tell ${owner}`} yet.
          </p>
        </div>
      );
    case "waiting-elsewhere":
      return (
        <div className="flex flex-col gap-2 text-sm text-text">
          <StateLine instance={instance} />
          <p>
            It is registered and waiting for its first ping. It runs elsewhere: use Files in the
            table to download its files and run it where it can run.
          </p>
          <p className="text-text-muted">
            Once it answers this server, the sign-in steps appear here. Nothing to{" "}
            {isSelf ? "do" : `tell ${owner}`} yet.
          </p>
        </div>
      );
    case "failed":
      return (
        <div className="flex flex-col gap-2 text-sm text-text">
          <StateLine instance={instance} />
          <p>{detail ? `It stopped on the way: ${detail}` : "It stopped on the way."}</p>
          <p className="text-text-muted">
            Retry it from the table, or remove it and add it again. Nothing to{" "}
            {isSelf ? "do" : `tell ${owner}`} until it is running.
          </p>
        </div>
      );
    case "removing":
      return (
        <div className="flex flex-col gap-2 text-sm text-text">
          <StateLine instance={instance} />
          <p className="text-text-muted">
            It is being removed. Nothing to {isSelf ? "do" : "tell them"}.
          </p>
        </div>
      );
    case "asking":
      return (
        <div className="flex flex-col gap-2 text-sm text-text">
          <StateLine instance={instance} />
          <p className="text-text-muted">
            Ready. Asking the bridge whether {who} {isSelf ? "have" : "has"} signed in…
          </p>
        </div>
      );
    case "signed-in":
      return (
        <div className="flex flex-col gap-2 text-sm text-text">
          <StateLine instance={instance} />
          <p>
            Signed in as <span className="font-identifier">{phase.as}</span>. Nothing left to do:{" "}
            {isSelf ? "your" : "their"} {name} chats arrive as rooms.
          </p>
        </div>
      );
    case "sign-in":
      return (
        <SignInSteps
          instance={instance}
          offering={offering}
          type={type}
          phase={phase}
          isSelf={isSelf}
          bot={bot}
        />
      );
  }
}

function StateLine({ instance }: { instance: BridgeInstance }) {
  return (
    <div>
      <Badge status={instanceStateBadge(instance.state)}>
        {instanceStateLabel(instance.state)}
      </Badge>
    </div>
  );
}

function SignInSteps({
  instance,
  offering,
  type,
  phase,
  isSelf,
  bot,
}: {
  instance: BridgeInstance;
  offering: BridgeOffering;
  type: BridgeType | undefined;
  phase: Extract<NextStepsPhase, { kind: "sign-in" }>;
  isSelf: boolean;
  bot: string | undefined;
}) {
  const name = offering.name ?? offering.type;
  const owner = instance.user_id ?? "";
  const steps = signInSteps(type, bot ?? "its bot");
  const command = firstCommand(steps);
  // The manager starts the chat as the person where it can act as them (double puppeting), so
  // there is nothing to accept; a chat its bot started needs the bridge's command prefix.
  const chatStartedAsOwner = instance.chat_started_by === "owner";
  const [copied, setCopied] = useState(false);

  async function copyMessage() {
    try {
      await navigator.clipboard.writeText(nextStepsMessage(type, name, bot));
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable: the steps are on the page to select */
    }
  }

  const status =
    phase.asked === "no"
      ? isSelf
        ? "Ready, and you have not signed in yet."
        : `Ready, and ${owner} has not signed in yet.`
      : phase.asked === "not-reported"
        ? `Ready. This kind of bridge keeps who has signed in to itself${phase.detail ? ` (${phase.detail})` : ""}, so these steps stay here.`
        : `Ready. Whether ${isSelf ? "you have" : "they have"} signed in could not be asked${phase.detail ? ` (${phase.detail})` : ""}; the steps apply until ${isSelf ? "you have" : "they have"}.`;

  return (
    <div className="flex flex-col gap-3 text-sm text-text">
      <div className="flex flex-wrap items-center gap-2">
        <StateLine instance={instance} />
        <span className="text-text-muted">{status}</span>
      </div>

      {isSelf ? (
        <p className="rounded-md border border-info-border bg-info-bg px-3 py-2 text-info">
          This is you: {chatStartedAsOwner ? "open your chat with" : "accept the invite from"}{" "}
          {bot ? <CopyableId value={bot} /> : "your bridge's bot"} in your chat app
          {command ? (
            <>
              {" "}
              and send <code className="font-identifier">{command}</code>.
            </>
          ) : (
            " and follow its steps."
          )}
        </p>
      ) : (
        <p>
          Tell them: their {name} bridge is ready, and its bot
          {bot && (
            <>
              , <CopyableId value={bot} />,
            </>
          )}{" "}
          {chatStartedAsOwner
            ? "is in a direct chat started for them. They should open it, then:"
            : "has invited them to a direct chat. They should accept it, then:"}
        </p>
      )}

      {instance.chat_started_by === "bot" && (
        <p
          className="rounded-md border border-warning-border bg-warning-bg px-3 py-2 text-warning"
          data-testid="chat-started-by-bot"
        >
          That chat was started by the bot, and a {name} bridge only takes bare commands in a chat
          the person invited it to, so there {isSelf ? "you" : "they"} must prefix commands
          {type?.command_prefix ? (
            <>
              :{" "}
              <code className="font-identifier">
                {type.command_prefix} {command ?? "login"}
              </code>{" "}
              rather than <code className="font-identifier">{command ?? "login"}</code>.
            </>
          ) : (
            " with the bridge's command prefix (its documentation says which)."
          )}{" "}
          Or {isSelf ? "start" : "they can start"} a new direct chat with{" "}
          {bot ? <span className="font-identifier">{bot}</span> : "the bot"}: the bridge takes bare
          commands there. The bot has said the same in the chat.
        </p>
      )}

      {steps.length > 0 ? (
        <ol className="flex flex-col gap-2">
          {steps.map((step, i) => (
            <li key={i} className="flex gap-3">
              <span className="flex size-6 shrink-0 items-center justify-center rounded-full bg-accent-muted text-xs font-medium text-accent">
                {i + 1}
              </span>
              <span className="pt-0.5">{withInlineCode(step)}</span>
            </li>
          ))}
        </ol>
      ) : (
        <p className="text-text-muted">
          The catalogue has no steps for this kind of bridge. Bridges usually take a{" "}
          <code className="font-identifier">login</code> command in that chat, or have a sign-in
          page of their own{type?.docs_url ? "; its documentation says which" : ""}.
        </p>
      )}

      {type?.sign_in?.notes && (
        <p className="rounded-md border border-info-border bg-info-bg px-3 py-2 text-info">
          {type.sign_in.notes}
        </p>
      )}

      {instance.device_name && (
        <p className="text-text-muted" data-testid="device-name">
          In {name}&apos;s own list of linked devices, this bridge is named{" "}
          <span className="font-identifier">{instance.device_name}</span>. A link made before that
          name was set keeps its old name until the bridge is linked again.
        </p>
      )}

      {instance.last_key_withheld && (
        <p
          className="rounded-md border border-warning-border bg-warning-bg px-3 py-2 text-warning"
          data-testid="key-withheld"
        >
          {new Date(instance.last_key_withheld.at).toLocaleString()}:{" "}
          <span className="font-identifier">{instance.last_key_withheld.sender}</span>&apos;s chat
          app refused to share a message&apos;s keys with the bridge (
          <code className="font-identifier">{instance.last_key_withheld.code}</code>
          {instance.last_key_withheld.reason ? `, ${instance.last_key_withheld.reason}` : ""}), so
          the bridge could not read that message and said so in the chat.{" "}
          {instance.last_key_withheld.code === "m.unverified"
            ? instance.signed_bot_device
              ? `The bot's device ${instance.signed_bot_device} has been cross-signed since; a chat app that excludes insecure devices shares keys with it once it has seen that, so a message sent again now should go through.`
              : "The chat app excludes devices their owner has not cross-signed; the server is about to cross-sign the bot's device, after which a message sent again goes through."
            : "That code is the chat app's own rule for the bridge's device; its encryption settings say which."}
        </p>
      )}

      {instance.signed_bot_device && !instance.last_key_withheld && (
        <p className="text-text-muted" data-testid="signed-bot-device">
          The bot&apos;s device{" "}
          <span className="font-identifier">{instance.signed_bot_device}</span> is cross-signed by
          the bot&apos;s own identity, so a chat app that excludes insecure devices still shares
          keys with the bridge.
        </p>
      )}

      {bot && instance.chat_started_by !== "bot" && (
        <p className="text-text-muted">
          If the {chatStartedAsOwner ? "chat" : "invite"} is nowhere to be found,{" "}
          {isSelf ? "start" : "they can start"} a direct chat with{" "}
          <span className="font-identifier">{bot}</span> {isSelf ? "yourself" : "themselves"}: the
          bridge treats it the same way.
        </p>
      )}

      <div className="flex flex-wrap items-center gap-3">
        <Button type="button" variant="secondary" size="sm" onClick={copyMessage}>
          {copied ? (
            <Check size={14} aria-hidden="true" className="text-success" />
          ) : (
            <Copy size={14} aria-hidden="true" />
          )}
          {copied ? "Copied" : isSelf ? "Copy the steps" : "Copy as a message to send them"}
        </Button>
        {type?.docs_url && (
          <a
            href={type.docs_url}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1 text-accent hover:underline"
          >
            <ExternalLink size={14} aria-hidden="true" />
            {type.name ?? name} documentation
          </a>
        )}
      </div>
    </div>
  );
}
