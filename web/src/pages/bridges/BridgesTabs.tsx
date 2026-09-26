import { Link } from "@tanstack/react-router";
import { cn } from "@/lib/cn";

/**
 * The two views of Bridges (RFC 0017): the bridges this server offers, which is where an
 * administrator starts, and every appservice registration, which is where delivery and health are
 * looked at (each person's bridge is one) and where a bridge run by hand is registered. Links,
 * not tabs: each view has its own address.
 */
export function BridgesTabs({ current }: { current: "offerings" | "registrations" }) {
  const item = (active: boolean) =>
    cn(
      "-mb-px border-b-2 px-3 py-2 text-sm transition-colors duration-fast",
      active
        ? "border-accent font-medium text-accent"
        : "border-transparent text-text-muted hover:text-text",
      "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
    );
  return (
    <nav aria-label="Bridges views" className="mt-4 flex gap-1 border-b border-border">
      <Link to="/bridges" activeOptions={{ exact: true }} className={item(current === "offerings")}>
        Offered bridges
      </Link>
      <Link
        to="/bridges/registrations"
        activeOptions={{ exact: true }}
        className={item(current === "registrations")}
      >
        Registrations
      </Link>
    </nav>
  );
}
