import { useState, useSyncExternalStore } from "react";
import { Menu, Search, Sun, Moon, Laptop, LogOut, RefreshCw } from "lucide-react";
import { Root, Trigger, Portal, Content, Item, Separator } from "radix-ui/dropdown-menu";
import { cn } from "@/lib/cn";
import { applyTheme, readTheme, type Theme } from "@/lib/theme";
import { getSession, signOut, subscribeSession } from "@/lib/auth";
import { useClusterStatus } from "@/api/dashboard";

export interface TopBarProps {
  onOpenPalette: () => void;
  onOpenNavDrawer: () => void;
}

const themeIcons: Record<Theme, typeof Sun> = { light: Sun, dark: Moon, system: Laptop };

export function TopBar({ onOpenPalette, onOpenNavDrawer }: TopBarProps) {
  const session = useSyncExternalStore(subscribeSession, getSession, getSession);
  const { data: cluster, dataUpdatedAt, isFetching } = useClusterStatus();
  const [theme, setTheme] = useState<Theme>(() => readTheme());
  const ThemeIcon = themeIcons[theme];

  function cycleTheme() {
    const order: Theme[] = ["system", "light", "dark"];
    const next = order[(order.indexOf(theme) + 1) % order.length];
    applyTheme(next);
    setTheme(next);
  }

  return (
    <header className="flex h-14 items-center gap-3 border-b border-border bg-surface px-4">
      <button
        type="button"
        onClick={onOpenNavDrawer}
        className="rounded-sm p-2 text-text-muted hover:bg-surface-sunken lg:hidden"
        aria-label="Open navigation"
      >
        <Menu size={20} aria-hidden="true" />
      </button>

      <div className="flex items-center gap-2 text-sm font-medium text-text">
        <span>hs admin</span>
        <span className="text-text-faint" aria-hidden="true">
          &middot;
        </span>
        <span className="text-text-muted">
          {(cluster?.replica_count ?? 1) > 1
            ? `Cluster of ${cluster?.replica_count}`
            : "Single node"}
        </span>
      </div>

      <button
        type="button"
        onClick={onOpenPalette}
        className={cn(
          "ml-4 flex flex-1 max-w-md items-center gap-2 rounded-sm border border-border-strong bg-surface-sunken px-3 py-1.5 text-sm text-text-faint",
          "hover:border-border-strong hover:text-text-muted",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
        )}
      >
        <Search size={14} aria-hidden="true" />
        <span className="flex-1 text-left">Search or jump to...</span>
        <kbd className="rounded-xs border border-border bg-surface px-1 text-xs">⌘K</kbd>
      </button>

      <div className="ml-auto flex items-center gap-1">
        <span
          className="hidden items-center gap-1.5 rounded-xs px-2 py-1 text-xs text-text-muted sm:flex"
          aria-live="polite"
        >
          <RefreshCw
            size={12}
            aria-hidden="true"
            className={cn("text-text-faint", isFetching && "animate-spin")}
          />
          {dataUpdatedAt
            ? `Updated ${new Date(dataUpdatedAt).toLocaleTimeString()}`
            : "Polling every 30s"}
        </span>

        <button
          type="button"
          onClick={cycleTheme}
          aria-label={`Theme: ${theme}. Click to change.`}
          className="rounded-sm p-2 text-text-muted hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
        >
          <ThemeIcon size={16} aria-hidden="true" />
        </button>

        <Root>
          <Trigger asChild>
            <button
              type="button"
              className="flex items-center gap-2 rounded-sm px-2 py-1.5 text-sm text-text hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
            >
              <span className="flex size-7 items-center justify-center rounded-full bg-accent-muted text-xs font-semibold text-accent">
                {(session?.operator.name ?? "?").slice(0, 1).toUpperCase()}
              </span>
            </button>
          </Trigger>
          <Portal>
            <Content
              align="end"
              sideOffset={4}
              className="z-50 w-64 rounded-md border border-border bg-surface-raised p-1 shadow-2"
            >
              <div className="px-3 py-2">
                <p className="text-sm font-medium text-text">{session?.operator.name}</p>
                <p className="mt-1 text-xs text-text-muted">Scopes: {session?.scopes.join(", ")}</p>
              </div>
              <Separator className="my-1 h-px bg-border" />
              <Item
                onSelect={() => signOut()}
                className="flex cursor-pointer items-center gap-2 rounded-sm px-3 py-2 text-sm text-text outline-none data-[highlighted]:bg-surface-sunken"
              >
                <LogOut size={14} aria-hidden="true" />
                Sign out
              </Item>
            </Content>
          </Portal>
        </Root>
      </div>
    </header>
  );
}
