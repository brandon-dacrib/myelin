/**
 * Server-notice recipients: turning what an administrator types into local user IDs.
 *
 * A notice goes only to users on this server, so a bare username is completed with this
 * server's name, and an ID on another server is refused here rather than by the server (which
 * would refuse the whole notice, sending nothing).
 */

/** The shape of a Matrix user ID: `@localpart:server`. */
const USER_ID = /^@[^:\s]+:[^\s]+$/;
/** A bare username someone typed without the `@` and server. */
const BARE = /^@?[^:\s@]+$/;

export type ParsedRecipient = { userId: string } | { error: string };

/**
 * Parses one typed recipient. `serverName` is this server's name when it is known; without it
 * only the shape of the ID can be checked.
 */
export function parseRecipient(input: string, serverName?: string): ParsedRecipient {
  const text = input.trim();
  if (!text) return { error: "Type a user ID." };
  if (BARE.test(text)) {
    if (!serverName)
      return { error: `Type the full user ID, like @${text.replace(/^@/, "")}:server.` };
    return { userId: `@${text.replace(/^@/, "").toLowerCase()}:${serverName}` };
  }
  if (!USER_ID.test(text)) return { error: `"${text}" is not a user ID (@name:server).` };
  const server = text.slice(text.indexOf(":") + 1);
  if (serverName && server !== serverName) {
    return { error: `${text} is not on this server; notices only go to users on ${serverName}.` };
  }
  return { userId: text };
}

/** Splits a pasted list ("@a:x, @b:x @c:x") into the recipients it names. */
export function splitRecipients(input: string): string[] {
  return input
    .split(/[\s,;]+/)
    .map((part) => part.trim())
    .filter(Boolean);
}
