import type { BridgeInstance, BridgeLogins, BridgeOffering, BridgeType } from "@/api/bridges";
import { signInSteps } from "./bridge-catalogue";

/**
 * What an operator tells a person whose bridge the server set up for them (RFC 0017 sections 4.1
 * and 4.2): which bot is theirs, where the instance has got to, and whether there is anything
 * left for them to do. The server's manager sends the same steps into a direct chat with the
 * person when their bridge is ready; the operator relays them when that message was missed, which
 * is the usual reason someone asks. Pure functions; `bridge-next-steps.test.ts`.
 */

/** RFC 0017 section 3's localpart encoding: `a-z`, `0-9`, `.`, `-` and `/` as they are, anything else as `=` and its byte in hex. */
export function encodeLocalpart(localpart: string): string {
  return [...new TextEncoder().encode(localpart)]
    .map((b) => {
      const c = String.fromCharCode(b);
      return /[a-z0-9./-]/.test(c) ? c : `=${b.toString(16).padStart(2, "0")}`;
    })
    .join("");
}

function splitUserId(userId: string): { localpart: string; server: string } | undefined {
  const m = /^@([^:]+):(.+)$/.exec(userId);
  return m ? { localpart: m[1], server: m[2] } : undefined;
}

/**
 * A person's own bridge bot. The server says which (`BridgeInstance.bot`); for a server that does
 * not, RFC 0017 section 4.1 names it from the offering's front door: the front door's localpart,
 * `_`, the owner's localpart encoded, at the front door's server (`@whatsappbot_alice:example.org`).
 * Undefined for a shared instance, or when neither the bot nor a front door is known.
 */
export function instanceBotId(
  instance: Pick<BridgeInstance, "bot" | "user_id">,
  offering: Pick<BridgeOffering, "front_door">,
): string | undefined {
  if (instance.bot) return instance.bot;
  if (!instance.user_id || !offering.front_door) return undefined;
  const door = splitUserId(offering.front_door);
  const owner = splitUserId(instance.user_id);
  if (!door || !owner) return undefined;
  return `@${door.localpart}_${encodeLocalpart(owner.localpart)}:${door.server}`;
}

/** What `useAppserviceLogins` knows so far, reduced to what the phase needs. */
export interface LoginsState {
  status: "pending" | "error" | "success";
  data?: BridgeLogins;
}

/**
 * Where a person's bridge is, as it decides what the operator sees.
 *
 * - `setting-up`: this server is running it up; the steps come when it is ready.
 * - `waiting-elsewhere`: it runs elsewhere and waits for an administrator to start it.
 * - `failed`, `removing`: nothing to tell the person.
 * - `asking`: ready, and the bridge is being asked whether they have signed in.
 * - `sign-in`: ready and not signed in (`asked: "no"`), or whether they have could not be found
 *   out (`could-not`: the bridge did not answer; `not-reported`: this kind of bridge keeps it to
 *   itself). The steps are shown either way: they are what to do until the bridge says otherwise.
 * - `signed-in`: nothing left to do.
 */
export type NextStepsPhase =
  | { kind: "setting-up" }
  | { kind: "waiting-elsewhere" }
  | { kind: "failed" }
  | { kind: "removing" }
  | { kind: "asking" }
  | { kind: "sign-in"; asked: "no" | "could-not" | "not-reported"; detail?: string }
  | { kind: "signed-in"; as: string };

export function nextStepsPhase(
  instance: Pick<BridgeInstance, "state" | "appservice_id">,
  runtime: BridgeOffering["runtime"],
  logins: LoginsState,
): NextStepsPhase {
  switch (instance.state) {
    case "removing":
      return { kind: "removing" };
    case "failed":
      return { kind: "failed" };
    case "ready":
      break;
    default:
      return runtime === "elsewhere" ? { kind: "waiting-elsewhere" } : { kind: "setting-up" };
  }
  if (!instance.appservice_id) {
    return {
      kind: "sign-in",
      asked: "could-not",
      detail: "the server did not say which registration is theirs",
    };
  }
  if (logins.status === "pending") return { kind: "asking" };
  if (logins.status === "error" || !logins.data) {
    return { kind: "sign-in", asked: "could-not", detail: "the server could not ask the bridge" };
  }
  const data = logins.data;
  if (!data.supported) {
    return { kind: "sign-in", asked: "not-reported", detail: data.reason ?? undefined };
  }
  if (data.error) return { kind: "sign-in", asked: "could-not", detail: data.error.detail };
  const first = data.logins[0];
  if (data.signed_in && first) {
    return { kind: "signed-in", as: first.remote_name ?? first.remote_id };
  }
  return { kind: "sign-in", asked: "no" };
}

/** A phase in two or three words, for the line that opens the steps. */
export function phaseLabel(phase: NextStepsPhase, isSelf: boolean): string {
  switch (phase.kind) {
    case "setting-up":
      return "Setting up";
    case "waiting-elsewhere":
      return "Waiting to be run";
    case "failed":
      return "Failed";
    case "removing":
      return "Being removed";
    case "asking":
      return "Checking";
    case "sign-in":
      return isSelf ? "Ready: sign in" : "Ready: tell them how to sign in";
    case "signed-in":
      return "Signed in";
  }
}

/** The command the first step asks for (`login qr`), or undefined when there is no such step. */
export function firstCommand(steps: readonly string[]): string | undefined {
  const first = steps[0];
  if (!first) return undefined;
  const m = /`([^`]+)`/.exec(first);
  return m ? m[1] : undefined;
}

/**
 * The words to paste into a message to the person: that their bridge is ready, which bot has
 * invited them, the catalogue's steps with their bot filled in, and the catalogue's note.
 */
export function nextStepsMessage(
  type: Pick<BridgeType, "sign_in"> | undefined,
  name: string,
  bot: string | undefined,
): string {
  const steps = signInSteps(type, bot ?? "its bot");
  const lines = [
    bot
      ? `Your ${name} bridge is ready. Its bot, ${bot}, has invited you to a direct chat: accept it, then:`
      : `Your ${name} bridge is ready. Its bot has invited you to a direct chat: accept it, then:`,
    ...steps.map((step, i) => `${i + 1}. ${step}`),
  ];
  if (steps.length === 0) {
    lines.push("1. Send `login` to the bot in that chat and follow what it says.");
  }
  if (bot) {
    lines.push(`If you can't find the invite, start a direct chat with ${bot} yourself.`);
  }
  const notes = type?.sign_in?.notes;
  if (notes) lines.push(`Note: ${notes}`);
  return lines.join("\n");
}
