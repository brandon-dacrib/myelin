import { useState } from "react";
import { Check, Copy } from "lucide-react";
import { cn } from "@/lib/cn";

/**
 * Every identifier is copyable in one click (information-architecture.md #7).
 *
 * The button is named after what it copies: "Copy @alice:example.org". `label` names it after
 * what the value *is* instead ("Copy password"), for a value that should not be read aloud by
 * a screen reader or turn up in an accessibility tree just because it can be copied.
 */
export function CopyableId({
  value,
  label,
  className,
}: {
  value: string;
  label?: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable: identifier remains visible and selectable */
    }
  }

  return (
    <span className={cn("inline-flex items-center gap-1.5", className)}>
      <span className="font-identifier text-text">{value}</span>
      <button
        type="button"
        onClick={handleCopy}
        aria-label={copied ? "Copied" : `Copy ${label ?? value}`}
        className={cn(
          "rounded-xs p-0.5 text-text-faint hover:bg-surface-sunken hover:text-text",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
        )}
      >
        {copied ? (
          <Check size={12} aria-hidden="true" className="text-success" />
        ) : (
          <Copy size={12} aria-hidden="true" />
        )}
      </button>
    </span>
  );
}
