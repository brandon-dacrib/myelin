/** Words and small calculations for the Migration page (`pages/migration/MigrationPage.tsx`). */
import type { MigrationPhase, MigrationStream } from "@/api/migration";

type BadgeStatus = "success" | "warning" | "danger" | "info" | "muted" | "neutral";

/** How each status of `MigrationStatus.status` is shown. */
export const MIGRATION_STATUS_META: Record<MigrationPhase, { label: string; status: BadgeStatus }> =
  {
    idle: { label: "Not started", status: "neutral" },
    copying: { label: "Copying", status: "info" },
    paused: { label: "Paused", status: "muted" },
    ready_for_cutover: { label: "Ready for cutover", status: "success" },
    cutting_over: { label: "Cutting over", status: "info" },
    verifying: { label: "Verifying", status: "info" },
    completed: { label: "Completed", status: "success" },
    failed: { label: "Stopped on an error", status: "danger" },
    aborted: { label: "Aborted", status: "warning" },
  };

/** One stream of a migration, in the operator's words. */
export interface StreamInfo {
  label: string;
  /** One line: what it copies, and what that keeps working for people. */
  explanation: string;
}

/**
 * What each stream copies (`hs_compat::migration::Stream`, `MigrationStatus.streams[].name`), in
 * the order a migration copies them: each stream's rows refer only to rows of the streams
 * before it. `docs/compat/synapse-migration-runbook.md`'s "What moves" says the same table by
 * table.
 */
export const STREAMS: Record<string, StreamInfo> = {
  users: {
    label: "Accounts",
    explanation:
      "Every account with its password hash, administrator and deactivated flags, display name and avatar: people sign in with the passwords they have.",
  },
  devices: {
    label: "Devices",
    explanation: "Each account's signed-in devices, with their names and when they were last seen.",
  },
  access_tokens: {
    label: "Sessions (access tokens)",
    explanation:
      "The tokens signed-in apps hold, so nobody has to sign in again after the cutover.",
  },
  refresh_tokens: {
    label: "Refresh tokens",
    explanation:
      "The tokens apps use to renew their sessions, so nobody is signed out when a session's access token expires after the cutover.",
  },
  threepids: {
    label: "Email addresses and phone numbers",
    explanation: "The addresses people sign in by and are found by.",
  },
  external_ids: {
    label: "Sign-in identities",
    explanation:
      "Links from an SSO provider's identity to the account, so a sign-in through the same provider lands in the same account.",
  },
  account_data: {
    label: "Account data and room tags",
    explanation:
      "Each account's settings stored on the server: favourites and other room tags, ignored people, direct-message lists.",
  },
  e2e_keys: {
    label: "Device encryption keys",
    explanation:
      "Each device's identity keys, unclaimed one-time keys and fallback key: other people's apps find the same keys, so nobody has to verify anybody again.",
  },
  cross_signing: {
    label: "Cross-signing keys",
    explanation:
      "Each account's master, self-signing and user-signing keys with their signatures: verified devices and verified people stay verified.",
  },
  key_backups: {
    label: "Key backups",
    explanation:
      "Server-side backups of message keys, under the same version numbers: encrypted history stays readable on a new sign-in.",
  },
  to_device: {
    label: "Messages waiting for devices",
    explanation:
      "Room keys and requests sent to phones that were offline, delivered in their first sync here.",
  },
  push_rules: {
    label: "Notification rules (push rules)",
    explanation:
      "Each account's own notification rules and its changes to the default ones: people are notified about what they chose.",
  },
  pushers: {
    label: "Phones to notify (pushers)",
    explanation:
      "Where each account's notifications are sent, so phones keep being notified without opening the app.",
  },
  filters: {
    label: "Sync filters",
    explanation:
      "The filters apps registered, under the ids Synapse gave them, so an app's next sync works unchanged.",
  },
  registration_tokens: {
    label: "Registration tokens",
    explanation: "Tokens handed out before the migration still open this server.",
  },
  rooms: {
    label: "Rooms",
    explanation:
      "Every room this server's users created or joined, with every event since they joined, replayed in order through this server's own checks; aliases and the public directory too. A room joined on another server starts from the join.",
  },
  receipts: {
    label: "Read receipts",
    explanation: "Who has read up to where, public and private, so unread counts stay right.",
  },
  media: {
    label: "Media",
    explanation:
      "Files uploaded here, under the same mxc:// addresses, with their contents when Synapse's media store is mounted.",
  },
  remote_media: {
    label: "Other servers' media",
    explanation:
      "Files from other servers that Synapse had cached, with their contents when Synapse's media store is mounted: pictures people have already seen open without a fetch. An entry whose file is gone is left out and fetched again when needed.",
  },
  // Before 2026-10-01 a server copied room events as a stream of their own.
  events: {
    label: "Room events",
    explanation: "The messages and other events of each room.",
  },
};

/** What each stream copies, by wire name; kept for the places that need only the label. */
export const STREAM_LABELS: Record<string, string> = Object.fromEntries(
  Object.entries(STREAMS).map(([name, info]) => [name, info.label]),
);

/** A stream's label; a stream this build does not know is named in words, never by wire name. */
export function streamLabel(name: string | null | undefined): string {
  if (!name) return "Unknown";
  if (STREAMS[name]) return STREAMS[name].label;
  if (name === "migration") return "Migration";
  const words = name.replaceAll("_", " ");
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** One thing a migration leaves behind, and why. */
export interface NotMoved {
  title: string;
  detail: string;
}

/**
 * What a migration does not copy, from the runbook's "What does not move"
 * (`docs/compat/synapse-migration-runbook.md`), so an operator knows before starting.
 */
export const WHAT_DOES_NOT_MOVE: readonly NotMoved[] = [
  {
    title: "History from before a join on another server",
    detail:
      "For a room hosted on another server, only what Synapse held from its users' join onwards is copied; this server fetches older history from the other servers when someone scrolls back, as Synapse did. A room Synapse is still joining is skipped until Synapse has finished, and a room people were only invited to, or have all left, is skipped and logged: they join it again after the cutover.",
  },
  {
    title: "Thumbnails",
    detail:
      "Synapse's thumbnails, of its own media and of other servers', are not copied: this server makes its own the first time one is asked for.",
  },
  {
    title: "Presence",
    detail: "Who is online is how people are right now; it starts again as they come back.",
  },
  {
    title: "Unread counts",
    detail:
      "Unread counts and notification badges are not carried: nothing imported counts as a notification, so every room shows as read until the next message after the cutover. The receipts that decide what is read from then on are copied.",
  },
  {
    title: "Server-notice rooms",
    detail:
      "Synapse's are copied as the rooms they are, with their m.server_notice tag, so their history stays; but this server sends its notices from its own notices user, and the first notice to a person after the cutover opens a new Server Notices room beside the old one.",
  },
  {
    title: "Bridges' positions",
    detail:
      "Where each bridge had read up to in Synapse's streams names nothing here. A bridge starts reading this server's streams from where it is registered, as it does after any restart of its homeserver.",
  },
  {
    title: "Dehydrated devices",
    detail: "This server has none yet; a client that kept one makes it again.",
  },
  {
    title: "A registration under way",
    detail:
      "A registration token's limits and completed count come over; a sign-up that had presented the token on Synapse starts again here.",
  },
  {
    title: "Turned-off pushers and retired push rules",
    detail:
      "Pushers turned off in Synapse are left out, as are push rules of kinds this server does not have and changes to default rules the specification has retired; each is logged. A refresh token Synapse had already exchanged is left out too: it would be refused there as well.",
  },
  {
    title: "Room keys deleted from a backup after the copy",
    detail:
      "The cutover's last pass adds and updates backed-up room keys but does not delete them, so a key deleted in Synapse in between stays here.",
  },
  {
    title: "Rejected events and outliers",
    detail: "Events Synapse rejected or held outside a room's history stay out of it here too.",
  },
  {
    title: "Bridges",
    detail:
      "Appservice registrations are not read from Synapse's database. Add each bridge in Bridges, or list its registration file in the bootstrap file's appservices.registration_files.",
  },
];

/** How far a stream has got, `null` before it has been counted. */
export function streamFraction(stream: MigrationStream): number | null {
  if (stream.done) return 1;
  const total = stream.total_count;
  if (total == null) return null;
  if (total === 0) return 1;
  const handled =
    (stream.copied_count ?? 0) + (stream.skipped_count ?? 0) + (stream.failed_count ?? 0);
  return Math.min(1, handled / total);
}

/** `93_000` → `"2 minutes"`; rounded the way a person would say it. */
export function formatDuration(ms: number): string {
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return seconds <= 1 ? "a second" : `${seconds} seconds`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return minutes === 1 ? "a minute" : `${minutes} minutes`;
  const hours = Math.round(minutes / 60);
  return hours === 1 ? "an hour" : `${hours} hours`;
}
