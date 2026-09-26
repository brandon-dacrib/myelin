/**
 * The mock's administrator recovery link, the sibling of the first-run setup offer in
 * `handlers.ts`.
 *
 * Unlike setup, which a mock server has finished with (it has its administrator), a recovery
 * link is open by default: `npm run dev:mock` shows the page at
 * `/admin/recover#token=mock-recovery-token` with nothing to seed. Like the real link it works
 * once and for fifteen minutes from when it was issued, which the mock takes to be the first
 * time it is inspected. Both facts live in `sessionStorage`, so they survive a reload in the
 * same tab and are gone with it; tests clear them between cases with `resetMockRecovery`.
 */

/** The token the mock's recovery link carries. */
export const MOCK_RECOVERY_TOKEN = "mock-recovery-token";

/** Where the mock remembers that its link was used. */
export const MOCK_RECOVERY_USED_KEY = "hs-mock:recovery-used";

/** Where the mock remembers when its link was issued. */
export const MOCK_RECOVERY_ISSUED_AT_KEY = "hs-mock:recovery-issued-at";

/** How long the link lives, as on the real server. */
export const MOCK_RECOVERY_LIFETIME_MS = 15 * 60_000;

/** The administrators a recovery link may reset: two, so the page has a choice to show. */
export const recoveryAdministrators: { user_id: string }[] = [
  { user_id: "@admin:example.org" },
  { user_id: "@ops:example.org" },
];

function storage(): Storage | null {
  try {
    return globalThis.sessionStorage ?? null;
  } catch {
    return null;
  }
}

/** When the mock's link expires, issuing it now if it has not been asked about before. */
export function mockRecoveryExpiresAt(nowMs = Date.now()): number {
  const store = storage();
  const stored = Number(store?.getItem(MOCK_RECOVERY_ISSUED_AT_KEY));
  if (Number.isFinite(stored) && stored > 0) return stored + MOCK_RECOVERY_LIFETIME_MS;
  store?.setItem(MOCK_RECOVERY_ISSUED_AT_KEY, String(nowMs));
  return nowMs + MOCK_RECOVERY_LIFETIME_MS;
}

/** Whether the mock's link is still open: neither used nor past its expiry. */
export function mockRecoveryLinkOpen(nowMs = Date.now()): boolean {
  if (storage()?.getItem(MOCK_RECOVERY_USED_KEY)) return false;
  return nowMs < mockRecoveryExpiresAt(nowMs);
}

/** Marks the mock's link used, as a successful reset does. */
export function useMockRecoveryLink(): void {
  storage()?.setItem(MOCK_RECOVERY_USED_KEY, "1");
}

/** Forgets that the link was used or issued, so the next test starts with a fresh one. */
export function resetMockRecovery(): void {
  storage()?.removeItem(MOCK_RECOVERY_USED_KEY);
  storage()?.removeItem(MOCK_RECOVERY_ISSUED_AT_KEY);
}
