import { useState } from "react";
import { Check, Copy, Download } from "lucide-react";
import { Button } from "./ui/button/Button";

/** A code/YAML block with Copy and Download (flows.md flow 1 step 7: "Each block has Copy and Download"). */
export function CopyBlock({
  label,
  content,
  filename,
}: {
  label: string;
  content: string;
  filename: string;
}) {
  const [copied, setCopied] = useState(false);

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(content);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable */
    }
  }

  function handleDownload() {
    const blob = new Blob([content], { type: "text/plain" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
  }

  return (
    <div className="rounded-md border border-border bg-surface-sunken">
      <div className="flex items-center justify-between border-b border-border px-3 py-2">
        <span className="text-xs font-medium text-text-muted">{label}</span>
        <div className="flex gap-1">
          <Button variant="ghost" size="sm" onClick={handleCopy}>
            {copied ? (
              <Check size={14} aria-hidden="true" className="text-success" />
            ) : (
              <Copy size={14} aria-hidden="true" />
            )}
            Copy
          </Button>
          <Button variant="ghost" size="sm" onClick={handleDownload}>
            <Download size={14} aria-hidden="true" />
            Download
          </Button>
        </div>
      </div>
      <pre
        // A scrollable read-only region needs keyboard focus on narrow screens.
        // eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex
        tabIndex={0}
        role="region"
        aria-label={label}
        className="overflow-x-auto p-3 font-identifier text-xs text-text focus-visible:outline-2 focus-visible:outline-[var(--color-focus)]"
      >
        {content}
      </pre>
    </div>
  );
}
