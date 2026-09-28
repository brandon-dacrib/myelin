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

export interface NavItem {
  id: string;
  label: string;
  href: string;
  icon: ComponentType<{ size?: number; "aria-hidden"?: boolean | "true" | "false" }>;
  scope?: Scope;
  shortcut?: string;
}

/** Sidebar order: frequency of use, then severity of what can go wrong (information-architecture.md #3). */
export const navItems: NavItem[] = [
  { id: "overview", label: "Overview", href: "/", icon: LayoutDashboard, shortcut: "g o" },
  {
    id: "bridges",
    label: "Bridges",
    href: "/bridges",
    icon: Cable,
    scope: "bridges:read",
    shortcut: "g b",
  },
  {
    id: "users",
    label: "Users",
    href: "/users",
    icon: Users,
    scope: "admin:read",
    shortcut: "g u",
  },
  {
    id: "rooms",
    label: "Rooms",
    href: "/rooms",
    icon: DoorOpen,
    scope: "admin:read",
    shortcut: "g r",
  },
  { id: "reports", label: "Reports", href: "/reports", icon: Flag, scope: "moderation:read" },
  {
    id: "federation",
    label: "Federation",
    href: "/federation",
    icon: Globe,
    scope: "admin:read",
    shortcut: "g f",
  },
  { id: "media", label: "Media", href: "/media", icon: Image, scope: "admin:read" },
  {
    id: "statistics",
    label: "Statistics",
    href: "/statistics",
    icon: ChartLine,
    scope: "admin:read",
  },
  { id: "cluster", label: "Cluster", href: "/cluster", icon: Boxes, scope: "admin:read" },
  {
    id: "migration",
    label: "Migration",
    href: "/migration",
    icon: ArrowRightLeft,
    scope: "admin:write",
  },
  { id: "tasks", label: "Tasks", href: "/tasks", icon: ListChecks, scope: "admin:read" },
  { id: "audit", label: "Audit log", href: "/audit", icon: ScrollText, scope: "admin:read" },
  {
    id: "configuration",
    label: "Configuration",
    href: "/configuration",
    icon: SlidersHorizontal,
    scope: "admin:read",
    shortcut: "g c",
  },
  { id: "settings", label: "Settings", href: "/settings", icon: Settings, scope: "admin:read" },
];

/**
 * Views inside a section that the command palette offers by name, beside the sections
 * themselves: "Go to Registration tokens" is quicker than Settings and then a tab.
 */
export const subNavItems: Omit<NavItem, "icon">[] = [
  {
    id: "settings-registration-tokens",
    label: "Registration tokens",
    href: "/settings/registration-tokens",
    scope: "admin:read",
  },
  {
    id: "settings-server-notices",
    label: "Server notices",
    href: "/settings/server-notices",
    scope: "moderation:read",
  },
];
