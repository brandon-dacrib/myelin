import { Link } from "@tanstack/react-router";
import { cn } from "@/lib/cn";

/**
 * The views of "Invites and tokens" (information-architecture.md, Settings; the `/settings`
 * routes keep their addresses): invite links (registration tokens, as the Matrix spec calls
 * them), API tokens for scripts and bots (admin tokens), and server notices. Links, not tabs,
 * as in Bridges: each view has its own address, so it can be linked to, bookmarked and reached
 * from the command palette.
 */
export function SettingsTabs({
  current,
}: {
  current: "registration-tokens" | "admin-tokens" | "server-notices";
}) {
  const item = (active: boolean) =>
    cn(
      "-mb-px border-b-2 px-3 py-2 text-sm transition-colors duration-fast",
      active
        ? "border-accent font-medium text-accent"
        : "border-transparent text-text-muted hover:text-text",
      "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
    );
  return (
    <nav aria-label="Invites and tokens views" className="mt-4 flex gap-1 border-b border-border">
      <Link
        to="/settings/registration-tokens"
        aria-current={current === "registration-tokens" ? "page" : undefined}
        className={item(current === "registration-tokens")}
      >
        Invite links
      </Link>
      <Link
        to="/settings/admin-tokens"
        aria-current={current === "admin-tokens" ? "page" : undefined}
        className={item(current === "admin-tokens")}
      >
        API tokens
      </Link>
      <Link
        to="/settings/server-notices"
        aria-current={current === "server-notices" ? "page" : undefined}
        className={item(current === "server-notices")}
      >
        Server notices
      </Link>
    </nav>
  );
}
