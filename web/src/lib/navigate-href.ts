import type { useNavigate } from "@tanstack/react-router";

/**
 * Navigates to an href that came from API data (attention rows, audit
 * entries: docs/design/information-architecture.md "every identifier ... is
 * a link") rather than a statically-known route literal. The router's typed
 * `navigate` cannot verify a runtime string against its route union, so this
 * is the one sanctioned place that casts around it.
 */
export function navigateToHref(navigate: ReturnType<typeof useNavigate>, href: string): void {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any -- see doc comment above
  navigate({ to: href as any });
}
