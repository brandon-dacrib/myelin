/**
 * The kinds of account `User.user_type` can name, in words, as the server accepts them: `bot`
 * and `support` (Synapse's two values), or none for a person (sent as `null`; offered here as
 * "person"). Recording a kind changes nothing about what the account can do.
 */
export const USER_TYPE_OPTIONS = [
  { value: "person", label: "Person" },
  { value: "bot", label: "Bot" },
  { value: "support", label: "Support account" },
];

/** What the kind control means, in the words every form that sets it uses. */
export const USER_TYPE_HINT =
  "A bot is run by software, a support account by this server's staff. It is recorded and shown on the account; it does not change what the account can do.";

/** `User.user_type` as the select shows it: no kind is a person. */
export function userTypeValue(userType: string | null | undefined): string {
  return userType || "person";
}

/** The select's value as `UserUpdate.user_type` / `UserCreate.user_type` send it. */
export function userTypeWire(value: string): string | null {
  return value === "person" ? null : value;
}
