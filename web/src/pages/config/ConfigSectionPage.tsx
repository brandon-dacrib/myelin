/**
 * `/configuration/:section` — the form for one configuration section.
 *
 * The form is generated: every control comes from `GET /config/schema` by way
 * of `lib/config-model.ts`, so a new field on a Rust struct appears here as
 * soon as the server describes it, and nothing in this file lists settings by
 * name. The page's own job is the part a generated form cannot do on its own:
 *
 * - say where each value came from, and refuse to offer an edit the API will
 *   reject (a setting an `HS__` variable pins; the bootstrap-only `storage`
 *   section);
 * - hold the edits as a draft, show them as a diff, and only then send the
 *   RFC 7396 merge patch they add up to;
 * - land a `400`'s validation errors on the fields they belong to, and turn a
 *   `412` into "someone else changed this" rather than a lost update;
 * - say plainly, before saving, whether the change applies now or at the next
 *   restart.
 */
import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { Link, useParams, useRouterState } from "@tanstack/react-router";
import { ChevronLeft, Lock, TriangleAlert } from "lucide-react";
import {
  useConfigSchema,
  useConfigSection,
  useUpdateConfigSection,
  useValidateConfig,
  type ConfigSectionWithEtag,
  type ConfigValidateReport,
} from "@/api/config";
import type { ConfigOrigin, ConfigSchemaModel, JsonValue } from "@/api/config-schema";
import { classifyError } from "@/api/problem";
import {
  applyDraft,
  buildMergePatch,
  buildSectionModel,
  changeEntries,
  fieldErrorsFor,
  flattenFields,
  getPath,
  humanizeKey,
  settingRowId,
  type Draft,
  type FieldError,
  type SettingGroup,
} from "@/lib/config-model";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { ChangeReview } from "./ChangeReview";
import { ConfigHistory } from "./ConfigHistory";
import { SettingRow } from "./SettingRow";

export function ConfigSectionPage() {
  const { section } = useParams({ from: "/configuration/$section" });
  const sectionQuery = useConfigSection(section);
  const schemaQuery = useConfigSchema();

  if (!hasScope("admin:read")) {
    return (
      <div className="p-6">
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  if (sectionQuery.isLoading) {
    return (
      <div className="p-6">
        <SkeletonText lines={6} />
      </div>
    );
  }

  if (sectionQuery.isError || !sectionQuery.data) {
    return (
      <div className="p-6">
        <QueryProblemState
          error={sectionQuery.error}
          resource={`the ${section} configuration`}
          scope="admin:read"
          onRetry={() => sectionQuery.refetch()}
        />
      </div>
    );
  }

  return (
    // Keyed by section: moving between sections is a different form, and none
    // of the draft, the validation errors or the conflict state should follow.
    <SectionForm
      key={section}
      section={section}
      data={sectionQuery.data}
      schema={schemaQuery.data}
      schemaSettled={!schemaQuery.isLoading}
      onReread={() => void sectionQuery.refetch()}
    />
  );
}

interface SectionFormProps {
  section: string;
  data: ConfigSectionWithEtag;
  schema: ConfigSchemaModel | undefined;
  schemaSettled: boolean;
  onReread: () => void;
}

function SectionForm({ section, data, schema, schemaSettled, onReread }: SectionFormProps) {
  const hash = useRouterState({ select: (s) => s.location.hash });
  const canWrite = hasScope("admin:write");
  const update = useUpdateConfigSection();
  const validate = useValidateConfig();

  const [draft, setDraft] = useState<Draft>({});
  const [errors, setErrors] = useState<FieldError[]>([]);
  const [conflict, setConflict] = useState<string | null>(null);
  const [reviewOpen, setReviewOpen] = useState(false);
  const [report, setReport] = useState<ConfigValidateReport | undefined>();

  const values = data.section.values;
  const meta = schema?.sections.find((s) => s.name === section);
  const reloadable = meta?.reloadable ?? data.section.reloadable;
  const bootstrap = meta?.bootstrap ?? false;

  const model = useMemo<SettingGroup>(
    () =>
      schema
        ? buildSectionModel(schema, section, values)
        : { path: "", label: humanizeKey(section), fields: [], groups: [] },
    [schema, section, values],
  );
  const fields = useMemo(() => flattenFields(model), [model]);
  const changes = useMemo(() => changeEntries(fields, values, draft), [fields, values, draft]);
  // The settings whose pending edit actually changes something — which is not
  // every setting in the draft: typing a value back to what it already was
  // leaves an entry behind that changes nothing.
  const dirtyPaths = useMemo(() => new Set(changes.map((c) => c.path)), [changes]);
  const patch = useMemo(() => buildMergePatch(draft), [draft]);

  const errorByPath = useMemo(() => {
    const map = new Map<string, string>();
    for (const e of errors) if (!map.has(e.path)) map.set(e.path, e.detail);
    return map;
  }, [errors]);

  const setValue = useCallback((path: string, next: JsonValue | null) => {
    setDraft((current) => ({ ...current, [path]: next }));
    setErrors((current) => current.filter((e) => e.path !== path));
    setReport(undefined);
  }, []);

  const revert = useCallback((path: string) => {
    setDraft((current) => {
      const next = { ...current };
      delete next[path];
      return next;
    });
    setReport(undefined);
  }, []);

  function discardAll() {
    setDraft({});
    setErrors([]);
    setReport(undefined);
  }

  function runValidate() {
    validate.mutate(
      { [section]: applyDraft(values, draft) },
      {
        onSuccess: setReport,
        onError: () => toast({ title: "Could not check this configuration", variant: "danger" }),
      },
    );
  }

  function save() {
    setErrors([]);
    update.mutate(
      { section, patch, etag: data.etag },
      {
        onSuccess: () => {
          setDraft({});
          setConflict(null);
          setReport(undefined);
          setReviewOpen(false);
          toast({
            title: `${model.label} saved`,
            description: reloadable
              ? "Applied to the running server."
              : "Stored. It takes effect the next time this server restarts.",
          });
        },
        onError: handleSaveError,
      },
    );
  }

  function handleSaveError(err: unknown) {
    const { kind, problem } = classifyError(err);
    if (problem?.status === 412) {
      setConflict(problem.detail ?? "The server's copy of this section has moved on.");
      setReviewOpen(false);
      return;
    }
    if (problem?.status === 400 && problem.errors) {
      const mapped = fieldErrorsFor(problem.errors, section);
      setErrors(mapped);
      setReviewOpen(false);
      focusSetting(mapped[0]?.path);
      toast({ title: problem.title, description: problem.detail, variant: "danger" });
      return;
    }
    if (kind === "forbidden") {
      toast({ title: "Not allowed to change this section", variant: "danger" });
      return;
    }
    toast({
      title: problem?.title ?? "Could not save",
      description: problem?.detail,
      variant: "danger",
    });
  }

  // A deep link from the search on the index page (…#setting-password-enabled).
  useEffect(() => {
    if (!hash || fields.length === 0) return;
    // `?.()`: jsdom (and any non-browser host) has no scrollIntoView, and a
    // missing scroll must not take the page down with it.
    document.getElementById(hash)?.scrollIntoView?.({ block: "center" });
  }, [hash, fields.length]);

  const locked = bootstrap || !canWrite || !schema;
  const lockedReason = bootstrap
    ? undefined
    : !canWrite
      ? "Changing configuration needs admin:write."
      : !schema
        ? "This server does not serve GET /config/schema, so there is no form to render."
        : undefined;

  return (
    <div className="mx-auto max-w-[68rem] p-6 pb-28">
      <Link
        to="/configuration"
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Configuration
      </Link>

      <div className="mt-2 flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-xl text-text">{model.label}</h1>
            {bootstrap ? (
              <Badge status="muted">Bootstrap only</Badge>
            ) : reloadable ? (
              <Badge status="success">Reloadable</Badge>
            ) : (
              <Badge status="neutral" hideIcon>
                Restart required
              </Badge>
            )}
          </div>
          <p className="font-identifier text-xs text-text-faint">{section}</p>
          {model.summary && (
            <p className="mt-2 max-w-2xl text-sm text-text-muted">{model.summary}</p>
          )}
        </div>
        <dl className="flex flex-wrap gap-x-6 gap-y-1 text-xs text-text-muted">
          <div className="flex gap-1">
            <dt>Highest precedence:</dt>
            <dd className="font-identifier text-text">{data.section.source}</dd>
          </div>
          {reloadable && (
            <div className="flex gap-1">
              <dt>Last reloaded:</dt>
              <dd className="text-text">
                <RelativeTime at={data.section.last_reloaded_at} />
              </dd>
            </div>
          )}
        </dl>
      </div>

      {bootstrap && (
        <Notice
          tone="muted"
          icon={<Lock size={16} aria-hidden="true" />}
          title="This section cannot be stored in the database"
        >
          <code className="font-identifier">{section}</code> says where the database is, so it is
          read before there is a database to read it from. Set it on the command line, in an{" "}
          <code className="font-identifier">HS__</code> environment variable, or in the bootstrap
          file. It is shown here so you can see what this replica is actually running with.
        </Notice>
      )}

      {!bootstrap && !reloadable && (
        <Notice tone="info" title="Changes here take effect at the next restart">
          Saving stores the new value straight away, but this section is not reloadable — the
          running process keeps the old one until it is restarted.
        </Notice>
      )}

      {!schema && schemaSettled && (
        <Notice
          tone="warning"
          icon={<TriangleAlert size={16} aria-hidden="true" />}
          title="No schema, so no form"
        >
          The controls on this page are generated from <code>GET /config/schema</code>, which this
          server did not answer. The effective values are below, read-only.
        </Notice>
      )}

      {conflict && (
        <Notice
          tone="warning"
          icon={<TriangleAlert size={16} aria-hidden="true" />}
          title="Someone else changed this section"
        >
          <p>{conflict}</p>
          <div className="mt-3 flex flex-wrap gap-2">
            <Button
              variant="secondary"
              size="sm"
              onClick={() => {
                setConflict(null);
                onReread();
              }}
            >
              Re-read the server&apos;s copy, keep my edits
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setConflict(null);
                discardAll();
                onReread();
              }}
            >
              Discard my edits
            </Button>
          </div>
        </Notice>
      )}

      {errors.length > 0 && (
        <Notice
          tone="danger"
          icon={<TriangleAlert size={16} aria-hidden="true" />}
          title={`The server rejected ${errors.length} setting${errors.length === 1 ? "" : "s"}`}
        >
          <ul className="mt-1 flex flex-col gap-1">
            {errors.map((e, index) => (
              <li key={`${e.path}-${index}`}>
                <button
                  type="button"
                  className="text-left text-sm text-danger underline hover:no-underline"
                  onClick={() => focusSetting(e.path)}
                >
                  <span className="font-identifier">{e.path || section}</span> — {e.detail}
                </button>
              </li>
            ))}
          </ul>
        </Notice>
      )}

      {fields.length === 0 ? (
        <div className="mt-6">
          <RawValues values={values} />
        </div>
      ) : (
        <div className="mt-6">
          <GroupView
            group={model}
            heading={null}
            values={values}
            draft={draft}
            dirtyPaths={dirtyPaths}
            errorByPath={errorByPath}
            origins={schema?.origins ?? {}}
            locked={locked}
            lockedReason={lockedReason}
            onChange={setValue}
            onRevert={revert}
          />
        </div>
      )}

      <div className="mt-10">
        <ConfigHistory section={section} />
      </div>

      {changes.length > 0 && (
        <div className="fixed inset-x-0 bottom-0 z-40 border-t border-border bg-surface-raised shadow-2">
          <div className="mx-auto flex max-w-[68rem] flex-wrap items-center justify-between gap-3 px-6 py-3">
            <p className="text-sm text-text">
              {changes.length} unsaved change{changes.length === 1 ? "" : "s"}
              {!reloadable && <span className="text-text-muted"> · takes effect on restart</span>}
            </p>
            <div className="flex gap-2">
              <Button variant="ghost" onClick={discardAll}>
                Discard
              </Button>
              <Button onClick={() => setReviewOpen(true)}>Review and save</Button>
            </div>
          </div>
        </div>
      )}

      <ChangeReview
        open={reviewOpen}
        onOpenChange={setReviewOpen}
        sectionLabel={model.label}
        reloadable={reloadable}
        changes={changes}
        patch={patch}
        report={report}
        validating={validate.isPending}
        saving={update.isPending}
        onValidate={runValidate}
        onSave={save}
      />
    </div>
  );
}

interface GroupViewProps {
  group: SettingGroup;
  heading: string | null;
  values: Record<string, JsonValue> | undefined;
  draft: Draft;
  dirtyPaths: ReadonlySet<string>;
  errorByPath: Map<string, string>;
  origins: Record<string, ConfigOrigin>;
  locked: boolean;
  lockedReason?: string;
  onChange: (path: string, value: JsonValue | null) => void;
  onRevert: (path: string) => void;
}

function GroupView({
  group,
  heading,
  values,
  draft,
  dirtyPaths,
  errorByPath,
  origins,
  locked,
  lockedReason,
  onChange,
  onRevert,
}: GroupViewProps) {
  return (
    <section aria-label={heading ?? undefined}>
      {heading && (
        <>
          <h2 className="text-md font-medium text-text">{heading}</h2>
          {group.summary && <p className="mt-1 text-sm text-text-muted">{group.summary}</p>}
        </>
      )}
      {group.fields.length > 0 && (
        <div
          className={`${heading ? "mt-3" : ""} divide-y divide-border rounded-md border border-border bg-surface`}
        >
          {group.fields.map((field) => (
            <SettingRow
              key={field.path}
              field={field}
              effective={getPath(values, field.path)}
              draftValue={draft[field.path]}
              edited={field.path in draft}
              dirty={dirtyPaths.has(field.path)}
              origin={origins[field.fullPath]}
              error={errorByPath.get(field.path)}
              locked={locked}
              lockedReason={lockedReason}
              onChange={(next) => onChange(field.path, next)}
              onRevert={() => onRevert(field.path)}
              onReset={() => onChange(field.path, null)}
            />
          ))}
        </div>
      )}
      {group.groups.map((child) => (
        <div key={child.path} className="mt-8">
          <GroupView
            group={child}
            heading={child.label}
            values={values}
            draft={draft}
            dirtyPaths={dirtyPaths}
            errorByPath={errorByPath}
            origins={origins}
            locked={locked}
            lockedReason={lockedReason}
            onChange={onChange}
            onRevert={onRevert}
          />
        </div>
      ))}
    </section>
  );
}

/** The fallback when there is no schema to generate a form from: the values, as they are. */
function RawValues({ values }: { values: Record<string, JsonValue> | undefined }) {
  if (Object.keys(values ?? {}).length === 0) {
    return <p className="text-sm text-text-muted">This section sets nothing.</p>;
  }
  return (
    <pre className="overflow-x-auto rounded-md border border-border bg-surface-sunken p-4 font-mono text-sm text-text">
      {JSON.stringify(values, null, 2)}
    </pre>
  );
}

interface NoticeProps {
  tone: "info" | "warning" | "danger" | "muted";
  title: string;
  icon?: ReactNode;
  children: ReactNode;
}

const NOTICE_TONES: Record<NoticeProps["tone"], string> = {
  info: "border-info-border bg-info-bg",
  warning: "border-warning-border bg-warning-bg",
  danger: "border-danger-border bg-danger-bg",
  muted: "border-border bg-surface-sunken",
};

function Notice({ tone, title, icon, children }: NoticeProps) {
  return (
    <div className={`mt-4 flex items-start gap-2 rounded-md border p-4 ${NOTICE_TONES[tone]}`}>
      {icon && <span className="mt-0.5 shrink-0 text-text-muted">{icon}</span>}
      <div className="min-w-0">
        <p className="text-sm font-medium text-text">{title}</p>
        <div className="mt-1 text-sm text-text-muted">{children}</div>
      </div>
    </div>
  );
}

/** Moves the operator to the setting a validation error is about. */
function focusSetting(path: string | undefined) {
  if (!path) return;
  const row = document.getElementById(settingRowId(path));
  if (!row) return;
  row.scrollIntoView?.({ block: "center" });
  row.querySelector<HTMLElement>("input:not([type=hidden]), textarea, select, button")?.focus();
}
