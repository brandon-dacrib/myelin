import type { ComponentType } from "react";
import {
  LayoutDashboard,
  Cable,
  Users,
  DoorOpen,
  Flag,
  Globe,
  Image,
  Boxes,
  ArrowRightLeft,
  ScrollText,
  Settings,
  ChartLine,
  ListChecks,
  SlidersHorizontal,
} from "lucide-react";
import type { Scope } from "@/lib/auth";

/**
 * The sidebar's groups, in order. Fourteen sections in one flat list gave a new operator no way
 * to tell the pages they will open every day (people, rooms, bridges) from the ones they open
 * when something is wrong or when the server itself is being changed; the headings say which is
 * which, and the Overview stands alone above them.
 */
export type NavGroup = "manage" | "watch" | "server";

export const NAV_GROUP_LABELS: Record<NavGroup, string> = {
  manage: "Manage",
  watch: "Watch",
  server: "Server",
};

export interface NavItem {
  id: string;
  label: string;
  href: string;
  icon: ComponentType<{ size?: number; "aria-hidden"?: boolean | "true" | "false" }>;
  scope?: Scope;
  shortcut?: string;
  /** The sidebar group; the Overview has none and sits above them all. */
  group?: NavGroup;
}

/** Sidebar order: by group, then frequency of use, then severity of what can go wrong (information-architecture.md #3). */
export const navItems: NavItem[] = [
  { id: "overview", label: "Overview", href: "/", icon: LayoutDashboard, shortcut: "g o" },
  {
    id: "users",
    label: "Users",
    href: "/users",
    icon: Users,
    scope: "admin:read",
    shortcut: "g u",
    group: "manage",
  },
  {
    id: "rooms",
    label: "Rooms",
    href: "/rooms",
    icon: DoorOpen,
    scope: "moderation:read",
    shortcut: "g r",
    group: "manage",
  },
  {
    id: "bridges",
    label: "Bridges",
    href: "/bridges",
    icon: Cable,
    scope: "bridges:read",
    shortcut: "g b",
    group: "manage",
  },
  {
    id: "reports",
    label: "Reports",
    href: "/reports",
    icon: Flag,
    scope: "moderation:read",
    group: "manage",
  },
  {
    id: "media",
    label: "Media",
    href: "/media",
    icon: Image,
    scope: "moderation:read",
    group: "manage",
  },
  {
    id: "federation",
    label: "Federation",
    href: "/federation",
    icon: Globe,
    scope: "admin:read",
    shortcut: "g f",
    group: "watch",
  },
  {
    id: "statistics",
    label: "Statistics",
    href: "/statistics",
    icon: ChartLine,
    scope: "admin:read",
    group: "watch",
  },
  {
    id: "tasks",
    label: "Tasks",
    href: "/tasks",
    icon: ListChecks,
    scope: "admin:read",
    group: "watch",
  },
  {
    id: "audit",
    label: "Audit log",
    href: "/audit",
    icon: ScrollText,
    scope: "admin:read",
    group: "watch",
  },
  {
    id: "cluster",
    label: "Cluster",
    href: "/cluster",
    icon: Boxes,
    scope: "admin:read",
    group: "server",
  },
  {
    id: "migration",
    label: "Migration",
    href: "/migration",
    icon: ArrowRightLeft,
    scope: "admin:read",
    group: "server",
  },
  {
    id: "configuration",
    label: "Configuration",
    href: "/configuration",
    icon: SlidersHorizontal,
    scope: "admin:read",
    shortcut: "g c",
    group: "server",
  },
  // "Settings" beside "Configuration" was two names for what read as one thing. This section is
  // the invite links, API tokens and server notices; the route keeps its address.
  {
    id: "settings",
    label: "Invites and tokens",
    href: "/settings",
    icon: Settings,
    scope: "admin:read",
    group: "server",
  },
];

/**
 * Views inside a section that the command palette offers by name, beside the sections
 * themselves: "Go to Invite links" is quicker than Invites and tokens and then a tab.
 */
export const subNavItems: Omit<NavItem, "icon">[] = [
  {
    id: "settings-registration-tokens",
    label: "Invite links",
    href: "/settings/registration-tokens",
    scope: "admin:read",
  },
  {
    id: "settings-admin-tokens",
    label: "API tokens",
    href: "/settings/admin-tokens",
    scope: "admin:read",
  },
  {
    id: "settings-server-notices",
    label: "Server notices",
    href: "/settings/server-notices",
    scope: "moderation:read",
  },
];
