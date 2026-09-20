/**
 * `/configuration` — every section of this server's configuration, and what
 * state each one is in.
 *
 * Configuration is in the database (`crates/hs-config/src/store.rs`), so this
 * is the place it gets changed: no YAML on a host, no restart to edit a rate
 * limit. The index's job is to answer, before you click into anything, which
 * sections have been changed from their defaults, which ones a restart is
 * needed for, and which ones the deployment has taken out of your hands.
 */
import { useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";
import {
  Activity,
  Boxes,
  Cable,
  Database,
  Gauge,
  Globe,
  Image,
  KeyRound,
  type LucideIcon,
  Network,
  RefreshCw,
  Search,
  Server,
  Settings2,
} from "lucide-react";
import {
  useConfigSchema,
  useConfigSections,
  useReloadConfig,
  type ConfigSection,
} from "@/api/config";
import type { ConfigSchemaModel } from "@/api/config-schema";
import {
  buildSectionModel,
  flattenFields,
  getPath,
  humanizeKey,
  isChanged,
  settingRowId,
  type SettingField,
  type SettingGroup,
} from "@/lib/config-model";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent, DialogTrigger } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { Input } from "@/components/ui/input/Input";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";

const SECTION_ICONS: Record<string, LucideIcon> = {
  server: Server,
  listeners: Network,
  storage: Database,
  media: Image,
  federation: Globe,
  rate_limits: Gauge,
  auth: KeyRound,
  appservices: Cable,
  telemetry: Activity,
  cluster: Boxes,
};

interface SectionCard {
  name: string;
  label: string;
  summary?: string;
  reloadable: boolean;
  bootstrap: boolean;
  source: string;
  lastReloadedAt?: string | null;
  settingCount: number;
  changedCount: number;
  pinnedCount: number;
  matches: SettingField[];
}

export function ConfigurationPage() {
  const canRead = hasScope("admin:read");
  const canWrite = hasScope("admin:write");
  const [query, setQuery] = useState("");

  const sections = useConfigSections();
  const schema = useConfigSchema();
  const reload = useReloadConfig();

  const cards = useMemo<SectionCard[]>(() => {
    if (!sections.data) return [];
    return sections.data.map((section) =>
      describeSection(section.name, section, schema.data, query),
    );
  }, [sections.data, schema.data, query]);

  const visible = query.trim()
    ? cards.filter(
        (card) =>
          card.matches.length > 0 ||
          card.label.toLowerCase().includes(query.trim().toLowerCase()) ||
          card.name.includes(query.trim().toLowerCase()),
      )
    : cards;

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Configuration</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-xl text-text">Configuration</h1>
          <p className="mt-1 max-w-2xl text-sm text-text-muted">
            Every setting this server runs on, kept in its own database rather than in a file on the
            host. Changes to a reloadable section apply straight away; the rest take effect at the
            next restart.
          </p>
        </div>
        <Dialog>
          <DialogTrigger asChild>
            <Button
              variant="secondary"
              disabled={!canWrite}
              title={!canWrite ? "Needs admin:write" : undefined}
              leadingIcon={<RefreshCw size={16} aria-hidden="true" />}
            >
              Re-read files
            </Button>
          </DialogTrigger>
          <DialogContent
            title="Re-read configuration files?"
            description="Re-reads the bootstrap file and hot-applies every reloadable section. Settings stored in the database still win over the file, so nothing you changed here is undone."
            footer={
              <>
                <DialogClose asChild>
                  <Button variant="secondary">Cancel</Button>
                </DialogClose>
                <DialogClose asChild>
                  <Button
                    onClick={() =>
                      reload.mutate(undefined, {
                        onSuccess: (report) =>
                          toast({
                            title:
                              report.errors.length > 0
                                ? `Reloaded with ${report.errors.length} problem${report.errors.length === 1 ? "" : "s"}`
                                : `Reloaded ${report.reloaded_sections.length} section${report.reloaded_sections.length === 1 ? "" : "s"}`,
                            description: report.reloaded_sections.join(", ") || undefined,
                            variant: report.errors.length > 0 ? "danger" : "default",
                          }),
                        onError: () =>
                          toast({ title: "Could not reload configuration", variant: "danger" }),
                      })
                    }
                  >
                    Re-read files
                  </Button>
                </DialogClose>
              </>
            }
          />
        </Dialog>
      </div>

      {schema.isError && (
        <div className="mt-4 rounded-md border border-warning-border bg-warning-bg p-4">
          <p className="text-sm font-medium text-warning">
            This server does not describe its configuration schema
          </p>
          <p className="mt-1 text-sm text-text-muted">
            The forms on these pages are generated from <code>GET /config/schema</code>. Without it,
            each section still shows the values it is running with, but they cannot be edited here.
          </p>
        </div>
      )}

      {sections.isError && (
        <div className="mt-6">
          <QueryProblemState
            error={sections.error}
            resource="configuration"
            scope="admin:read"
            onRetry={() => sections.refetch()}
          />
        </div>
      )}

      {!sections.isError && (
        <>
          <div className="mt-6 max-w-md">
            <label htmlFor="config-search" className="sr-only">
              Search settings
            </label>
            <div className="relative">
              <Search
                size={16}
                aria-hidden="true"
                className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-text-faint"
              />
              <Input
                id="config-search"
                type="search"
                value={query}
                placeholder="Search all settings, e.g. registration"
                className="pl-9"
                onChange={(e) => setQuery(e.target.value)}
              />
            </div>
          </div>

          {sections.isLoading ? (
            <div className="mt-6">
              <SkeletonText lines={8} />
            </div>
          ) : visible.length === 0 ? (
            <div className="mt-8">
              <EmptyState
                icon={<Settings2 aria-hidden="true" />}
                title="No settings match"
                description={`Nothing in the configuration matches "${query}".`}
              />
            </div>
          ) : (
            <ul className="mt-6 grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
              {visible.map((card) => (
                <SectionCardView key={card.name} card={card} />
              ))}
            </ul>
          )}
        </>
      )}
    </div>
  );
}

function SectionCardView({ card }: { card: SectionCard }) {
  const Icon = SECTION_ICONS[card.name] ?? Settings2;
  return (
    <li className="rounded-md border border-border bg-surface transition-colors duration-fast hover:border-border-strong">
      <div className="p-4">
        <div className="flex items-start gap-3">
          <Icon size={18} aria-hidden="true" className="mt-0.5 shrink-0 text-text-muted" />
          <div className="min-w-0 flex-1">
            <h2 className="text-md font-medium text-text">
              <Link
                to="/configuration/$section"
                params={{ section: card.name }}
                className="hover:text-accent hover:underline"
              >
                {card.label}
              </Link>
            </h2>
            <p className="font-identifier text-xs text-text-faint">{card.name}</p>
          </div>
        </div>

        {card.summary && <p className="mt-2 text-sm text-text-muted">{card.summary}</p>}

        <div className="mt-3 flex flex-wrap items-center gap-2">
          {card.bootstrap ? (
            <Badge status="muted">Bootstrap only</Badge>
          ) : card.reloadable ? (
            <Badge status="success">Reloadable</Badge>
          ) : (
            <Badge status="neutral" hideIcon>
              Restart required
            </Badge>
          )}
          {card.pinnedCount > 0 && (
            <Badge status="warning">{card.pinnedCount} pinned by environment</Badge>
          )}
        </div>

        <dl className="mt-3 flex flex-wrap gap-x-6 gap-y-1 text-xs text-text-muted">
          {card.settingCount > 0 && (
            <div className="flex gap-1">
              <dt>Changed from default:</dt>
              <dd className="text-text">
                {card.changedCount} of {card.settingCount}
              </dd>
            </div>
          )}
          <div className="flex gap-1">
            <dt>Highest precedence:</dt>
            <dd className="font-identifier text-text">{card.source}</dd>
          </div>
          {card.reloadable && (
            <div className="flex gap-1">
              <dt>Last reloaded:</dt>
              <dd className="text-text">
                <RelativeTime at={card.lastReloadedAt} />
              </dd>
            </div>
          )}
        </dl>

        {card.matches.length > 0 && (
          <ul className="mt-3 border-t border-border pt-3">
            {card.matches.slice(0, 5).map((field) => (
              <li key={field.path} className="py-0.5">
                <Link
                  to="/configuration/$section"
                  params={{ section: card.name }}
                  hash={settingRowId(field.path)}
                  className="font-identifier text-xs text-accent hover:underline"
                >
                  {field.fullPath}
                </Link>
              </li>
            ))}
            {card.matches.length > 5 && (
              <li className="py-0.5 text-xs text-text-muted">and {card.matches.length - 5} more</li>
            )}
          </ul>
        )}
      </div>
    </li>
  );
}

function describeSection(
  name: string,
  section: ConfigSection,
  schema: ConfigSchemaModel | undefined,
  query: string,
): SectionCard {
  const meta = schema?.sections.find((s) => s.name === name);
  const empty: SettingGroup = { path: "", label: humanizeKey(name), fields: [], groups: [] };
  const model = schema ? buildSectionModel(schema, name, section.values) : empty;
  const fields = flattenFields(model);
  const needle = query.trim().toLowerCase();

  let changed = 0;
  let pinned = 0;
  for (const field of fields) {
    const origin = schema?.origins[field.fullPath];
    if (origin === "environment") pinned += 1;
    if (isChanged(field, getPath(section.values, field.path), origin)) changed += 1;
  }

  return {
    name,
    label: model.label,
    summary: model.summary,
    reloadable: meta?.reloadable ?? section.reloadable,
    bootstrap: meta?.bootstrap ?? false,
    source: section.source,
    lastReloadedAt: section.last_reloaded_at,
    settingCount: fields.length,
    changedCount: changed,
    pinnedCount: pinned,
    matches: needle
      ? fields.filter(
          (f) =>
            f.fullPath.toLowerCase().includes(needle) ||
            f.label.toLowerCase().includes(needle) ||
            (f.summary?.toLowerCase().includes(needle) ?? false),
        )
      : [],
  };
}
