import { Cable, Hash, MessageCircle, Users, Webhook } from "lucide-react";
import { cn } from "@/lib/cn";

const ICONS = {
  messaging: MessageCircle,
  social: Users,
  irc: Hash,
  integrations: Webhook,
} as const;

const SIZES = {
  sm: "size-7 rounded-sm [&>svg]:size-4",
  md: "size-9 rounded-md [&>svg]:size-5",
  lg: "size-12 rounded-md [&>svg]:size-6",
} as const;

/**
 * The mark next to a bridge's name: one icon per catalogue category on the accent wash, or a
 * cable for an appservice that did not come through the catalogue. Decorative: the name is
 * always written beside it, so it carries no accessible name of its own.
 */
export function BridgeGlyph({
  category,
  size = "md",
  className,
}: {
  category?: string | null;
  size?: keyof typeof SIZES;
  className?: string;
}) {
  const Icon = (category && category in ICONS ? ICONS[category as keyof typeof ICONS] : Cable) as
    typeof Cable | typeof MessageCircle;
  return (
    <span
      aria-hidden="true"
      className={cn(
        "inline-flex shrink-0 items-center justify-center bg-accent-muted text-accent",
        SIZES[size],
        className,
      )}
    >
      <Icon strokeWidth={1.75} />
    </span>
  );
}
