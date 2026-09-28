/** Mock fixtures for a user's 3PIDs, linked identities, experimental features, account data and pushers. */
import type { ExternalId, Pusher, ThreePid } from "@/api/user-identity";

const iso = (agoMs: number) => new Date(Date.now() - agoMs).toISOString();

export const KNOWN_FEATURES = ["msc3575", "msc3881", "msc4222"];

export const userThreepids: Record<string, ThreePid[]> = {
  "@alice:example.org": [
    { medium: "email", address: "alice@example.org", added_at: iso(40 * 86_400_000) },
  ],
};

export const userExternalIds: Record<string, ExternalId[]> = {
  "@alice:example.org": [{ provider: "oidc-corp", external_id: "248289761001" }],
};

export const userFeatures: Record<string, Record<string, boolean>> = {
  "@alice:example.org": { msc4222: true },
};

export const userAccountData: Record<string, Record<string, Record<string, unknown>>> = {
  "@alice:example.org": {
    "m.direct": { "@admin:example.org": ["!dm:example.org"] },
    "im.vector.setting.breadcrumbs": { recent_rooms: ["!general:example.org"] },
  },
};

export const userPushers: Record<string, Pusher[]> = {
  "@alice:example.org": [
    {
      pushkey: "alice-phone-key",
      kind: "http",
      app_id: "im.vector.app.android",
      app_display_name: "Element",
      device_display_name: "Pixel 8",
      lang: "en",
      data: { url: "https://push.example.org/_matrix/push/v1/notify" },
    },
  ],
};
