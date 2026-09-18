import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { Outlet, useNavigate, useRouterState } from "@tanstack/react-router";
import { Sidebar } from "./Sidebar";
import { TopBar } from "./TopBar";
import { CommandPalette } from "./CommandPalette";
import { SignIn } from "./SignIn";
import { Sheet, SheetContent } from "../ui/sheet/Sheet";
import { Toaster } from "../ui/toast/Toaster";
import { getSession, subscribeSession } from "@/lib/auth";

const GO_TARGETS: Record<string, string> = {
  o: "/",
  b: "/bridges",
  u: "/users",
  r: "/rooms",
  f: "/federation",
};

export function AppShell() {
  const session = useSyncExternalStore(subscribeSession, getSession, getSession);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const navigate = useNavigate();
  const mainRef = useRef<HTMLDivElement>(null);
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const awaitingGRef = useRef(false);

  // Focus moves to the main region after a route change (accessibility.md #2).
  useEffect(() => {
    mainRef.current?.focus();
  }, [pathname]);

  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      const target = e.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);

      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen(true);
        return;
      }
      if (typing) return;

      if (e.key === "/") {
        e.preventDefault();
        setPaletteOpen(true);
        return;
      }
      if (awaitingGRef.current) {
        awaitingGRef.current = false;
        const dest = GO_TARGETS[e.key.toLowerCase()];
        if (dest) navigate({ to: dest });
        return;
      }
      if (e.key === "g") {
        awaitingGRef.current = true;
        setTimeout(() => {
          awaitingGRef.current = false;
        }, 800);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [navigate]);

  if (!session) return <SignIn />;

  return (
    <div className="flex h-screen flex-col bg-canvas">
      <TopBar
        onOpenPalette={() => setPaletteOpen(true)}
        onOpenNavDrawer={() => setDrawerOpen(true)}
      />
      <div className="flex flex-1 overflow-hidden">
        <aside className="hidden w-14 shrink-0 border-r border-border bg-surface lg:block xl:hidden">
          <Sidebar variant="rail" />
        </aside>
        <aside className="hidden w-60 shrink-0 border-r border-border bg-surface xl:block">
          <Sidebar variant="full" />
        </aside>
        <main id="main" ref={mainRef} tabIndex={-1} className="flex-1 overflow-y-auto outline-none">
          <Outlet />
        </main>
      </div>

      <Sheet open={drawerOpen} onOpenChange={setDrawerOpen}>
        <SheetContent side="left" title="hs admin">
          <Sidebar variant="full" onNavigate={() => setDrawerOpen(false)} />
        </SheetContent>
      </Sheet>

      <CommandPalette open={paletteOpen} onOpenChange={setPaletteOpen} />
      <Toaster />
    </div>
  );
}
