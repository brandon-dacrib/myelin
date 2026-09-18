import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { Root, Portal, Overlay, Content, Title } from "radix-ui/dialog";
import { Search } from "lucide-react";
import { cn } from "@/lib/cn";
import { navItems } from "./nav";
import { hasScope } from "@/lib/auth";
import { useAppservices, deriveDisplayName } from "@/api/bridges";

interface PaletteItem {
  id: string;
  label: string;
  hint?: string;
  onSelect: () => void;
}

export interface CommandPaletteProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/** Jump to any nav section or bridge by name; ⌘K or `/` (information-architecture.md #6). */
export function CommandPalette({ open, onOpenChange }: CommandPaletteProps) {
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [activeIndex, setActiveIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const { data: bridgePage } = useAppservices({ limit: 50 });

  // Focus only (no setState): reset of query/activeIndex on open happens in
  // handleOpenChange below, not here, so state resets stay an event
  // response rather than a cascading render from inside an effect.
  useEffect(() => {
    if (open) queueMicrotask(() => inputRef.current?.focus());
  }, [open]);

  function handleOpenChange(next: boolean) {
    if (next) {
      setQuery("");
      setActiveIndex(0);
    }
    onOpenChange(next);
  }

  const items = useMemo<PaletteItem[]>(() => {
    const navChoices: PaletteItem[] = navItems
      .filter((n) => !n.scope || hasScope(n.scope))
      .map((n) => ({
        id: `nav-${n.id}`,
        label: `Go to ${n.label}`,
        onSelect: () => navigate({ to: n.href }),
      }));
    const bridgeChoices: PaletteItem[] = (bridgePage?.items ?? [])
      .filter((b): b is typeof b & { id: string } => Boolean(b.id))
      .map((b) => ({
        id: `bridge-${b.id}`,
        label: deriveDisplayName(b),
        hint: "Bridge",
        onSelect: () => navigate({ to: "/bridges/$bridgeId", params: { bridgeId: b.id } }),
      }));
    const all = [...navChoices, ...bridgeChoices];
    if (!query.trim()) return all;
    const q = query.toLowerCase();
    return all.filter((i) => i.label.toLowerCase().includes(q));
  }, [query, bridgePage, navigate]);

  function commit(item: PaletteItem | undefined) {
    if (!item) return;
    item.onSelect();
    onOpenChange(false);
  }

  return (
    <Root open={open} onOpenChange={handleOpenChange}>
      <Portal>
        <Overlay className="fixed inset-0 z-50 bg-black/40" />
        <Content
          className={cn(
            "fixed left-1/2 top-24 z-50 w-[calc(100vw-2rem)] max-w-lg -translate-x-1/2 overflow-hidden",
            "rounded-lg border border-border bg-surface-raised shadow-4",
          )}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setActiveIndex((i) => Math.min(i + 1, items.length - 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setActiveIndex((i) => Math.max(i - 1, 0));
            } else if (e.key === "Enter") {
              e.preventDefault();
              commit(items[activeIndex]);
            }
          }}
        >
          <Title className="sr-only">Command palette</Title>
          <div className="flex items-center gap-2 border-b border-border px-4">
            <Search size={16} aria-hidden="true" className="text-text-faint" />
            <input
              ref={inputRef}
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
                setActiveIndex(0);
              }}
              placeholder="Jump to a bridge, or go to a section..."
              aria-label="Command palette"
              role="combobox"
              aria-expanded="true"
              aria-controls="command-palette-list"
              className="h-12 flex-1 bg-transparent text-base text-text outline-none placeholder:text-text-faint"
            />
          </div>
          <div id="command-palette-list" role="listbox" className="max-h-80 overflow-y-auto p-2">
            {items.length === 0 && (
              <div className="px-3 py-6 text-center text-sm text-text-muted">No matches</div>
            )}
            {items.map((item, i) => (
              <div key={item.id} role="option" aria-selected={i === activeIndex}>
                <button
                  type="button"
                  onMouseEnter={() => setActiveIndex(i)}
                  onClick={() => commit(item)}
                  className={cn(
                    "flex w-full items-center justify-between rounded-sm px-3 py-2 text-left text-sm",
                    i === activeIndex ? "bg-accent-muted text-accent" : "text-text",
                  )}
                >
                  <span>{item.label}</span>
                  {item.hint && <span className="text-xs text-text-faint">{item.hint}</span>}
                </button>
              </div>
            ))}
          </div>
        </Content>
      </Portal>
    </Root>
  );
}
