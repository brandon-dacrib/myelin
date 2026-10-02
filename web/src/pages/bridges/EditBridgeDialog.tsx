import { useId, useState, type FormEvent } from "react";
import { Plus, X } from "lucide-react";
import {
  useUpdateAppservice,
  type AppService,
  type AppserviceNamespaces,
  type AppserviceUpdate,
  type NamespaceRule,
} from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { Switch } from "@/components/ui/switch/Switch";

type Category = "users" | "aliases" | "rooms";

const CATEGORIES: { key: Category; label: string; hint: string; example: string }[] = [
  {
    key: "users",
    label: "User namespaces",
    hint: "Regular expressions over user IDs the bridge speaks for. A puppet it makes for somebody on the other network has an ID like this.",
    example: "@whatsapp_.*:example\\.org",
  },
  {
    key: "aliases",
    label: "Alias namespaces",
    hint: "Room aliases the bridge may create and answer for.",
    example: "#whatsapp_.*:example\\.org",
  },
  {
    key: "rooms",
    label: "Room namespaces",
    hint: "Room IDs the bridge is told about whether or not one of its users is in them. Rarely needed.",
    example: "!.*:example\\.org",
  },
];

/** The dialog's own copy of the rules, each with a key that survives reordering. */
interface RuleRow extends NamespaceRule {
  key: number;
}

/**
 * Edits the parts of a registration an administrator changes after the bridge is set up
 * (`PATCH /appservices/{id}`): where the server reaches it, whether it is rate limited, and the
 * namespaces it speaks for. Not the tokens (rotate them instead) and not the id or bot name,
 * which the server does not let a patch change.
 *
 * The whole namespaces object is sent, with every rule: the server applies a merge patch, and a
 * merge patch replaces a list rather than merging it.
 */
export function EditBridgeDialog({
  bridge,
  name,
  open,
  onOpenChange,
}: {
  bridge: AppService;
  name: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const update = useUpdateAppservice();
  const rateLabelId = useId();
  const rateHintId = useId();
  const initial = readNamespaces(bridge.namespaces);
  const [url, setUrl] = useState(bridge.url ?? "");
  const [rateLimited, setRateLimited] = useState(Boolean(bridge.rate_limited));
  const [rules, setRules] = useState<Record<Category, RuleRow[]>>(initial);
  const [nextKey, setNextKey] = useState(
    initial.users.length + initial.aliases.length + initial.rooms.length,
  );
  const [errors, setErrors] = useState<{ url?: string; namespaces?: string; form?: string }>({});

  function reset() {
    setUrl(bridge.url ?? "");
    setRateLimited(Boolean(bridge.rate_limited));
    setRules(readNamespaces(bridge.namespaces));
    setErrors({});
  }

  function handleOpenChange(next: boolean) {
    if (!next) reset();
    onOpenChange(next);
  }

  function addRule(category: Category) {
    setRules((r) => ({
      ...r,
      [category]: [...r[category], { key: nextKey, regex: "", exclusive: true }],
    }));
    setNextKey((k) => k + 1);
  }

  function setRule(category: Category, key: number, change: Partial<NamespaceRule>) {
    setRules((r) => ({
      ...r,
      [category]: r[category].map((rule) => (rule.key === key ? { ...rule, ...change } : rule)),
    }));
  }

  function removeRule(category: Category, key: number) {
    setRules((r) => ({ ...r, [category]: r[category].filter((rule) => rule.key !== key) }));
  }

  function patch(): AppserviceUpdate {
    const body: AppserviceUpdate = {};
    const nextUrl = url.trim() === "" ? null : url.trim();
    if (nextUrl !== (bridge.url ?? null)) body.url = nextUrl;
    if (rateLimited !== Boolean(bridge.rate_limited)) body.rate_limited = rateLimited;
    const next = writeNamespaces(rules);
    if (JSON.stringify(next) !== JSON.stringify(writeNamespaces(initial))) body.namespaces = next;
    return body;
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setErrors({});
    const empty = (Object.keys(rules) as Category[]).some((c) =>
      rules[c].some((rule) => rule.regex.trim() === ""),
    );
    if (empty) {
      setErrors({ namespaces: "Every rule needs a pattern; remove the ones you do not want." });
      return;
    }
    const body = patch();
    if (Object.keys(body).length === 0) {
      handleOpenChange(false);
      return;
    }
    try {
      await update.mutateAsync({ id: bridge.id ?? "", patch: body });
      onOpenChange(false);
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const { problem } = err;
        const first = problem.errors?.[0];
        const message = first?.detail ?? problem.detail ?? problem.title ?? "The server refused.";
        if (first?.pointer?.startsWith("/url")) setErrors({ url: message });
        else if (first?.pointer?.startsWith("/namespaces") || problem.status === 409)
          setErrors({ namespaces: message });
        else setErrors({ form: message });
        return;
      }
      setErrors({ form: "Couldn’t reach the server." });
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title={`Edit ${name}`}
        description="Where this server reaches the bridge, whether it is rate limited, and the IDs it speaks for. The bridge's tokens and bot name are not changed here."
      >
        <form className="flex flex-col gap-5" onSubmit={handleSubmit} noValidate>
          <Field
            label="URL"
            hint="Where this server sends the bridge its events. Empty means the bridge is never pushed to; it can still act through the API."
            error={errors.url}
          >
            {(fieldProps) => (
              <Input
                {...fieldProps}
                type="url"
                autoComplete="off"
                spellCheck={false}
                className="font-identifier"
                placeholder="http://bridge.internal:29318"
                value={url}
                onChange={(e) => setUrl(e.target.value)}
              />
            )}
          </Field>
          <div className="flex items-start justify-between gap-4">
            <div>
              <p id={rateLabelId} className="text-sm font-medium text-text">
                Rate limited
              </p>
              <p id={rateHintId} className="text-sm text-text-muted">
                Whether the server's per-user limits apply to this bridge and the users it makes.
                Off is usual for a bridge, which sends on behalf of many people at once.
              </p>
            </div>
            <Switch
              checked={rateLimited}
              onCheckedChange={setRateLimited}
              aria-labelledby={rateLabelId}
              aria-describedby={rateHintId}
            />
          </div>

          <fieldset className="flex flex-col gap-4">
            <legend className="text-sm font-medium text-text">Namespaces</legend>
            <p className="-mt-2 text-sm text-text-muted">
              Which user IDs, room aliases and room IDs are the bridge&apos;s. An exclusive rule
              means nobody but this bridge may create them; the server refuses a rule that overlaps
              another bridge&apos;s exclusive one.
            </p>
            {CATEGORIES.map((category) => (
              <div key={category.key}>
                <div className="flex items-center justify-between gap-2">
                  <p className="text-sm font-medium text-text">{category.label}</p>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    leadingIcon={<Plus size={14} aria-hidden="true" />}
                    onClick={() => addRule(category.key)}
                  >
                    Add rule
                  </Button>
                </div>
                <p className="text-xs text-text-muted">{category.hint}</p>
                {rules[category.key].length === 0 ? (
                  <p className="mt-1 text-xs text-text-faint">None.</p>
                ) : (
                  <ul className="mt-2 flex flex-col gap-2">
                    {rules[category.key].map((rule) => (
                      <li key={rule.key} className="flex items-center gap-2">
                        <Input
                          aria-label={`${category.label} pattern`}
                          spellCheck={false}
                          className="font-identifier"
                          placeholder={category.example}
                          value={rule.regex}
                          onChange={(e) =>
                            setRule(category.key, rule.key, { regex: e.target.value })
                          }
                        />
                        <span className="flex shrink-0 items-center gap-2 text-xs text-text-muted">
                          <Switch
                            checked={Boolean(rule.exclusive)}
                            onCheckedChange={(exclusive) =>
                              setRule(category.key, rule.key, { exclusive })
                            }
                            aria-label={`${category.label} rule is exclusive`}
                          />
                          <span aria-hidden="true">Exclusive</span>
                        </span>
                        <Button
                          type="button"
                          variant="ghost"
                          size="sm"
                          aria-label={`Remove ${category.label.toLowerCase()} rule`}
                          onClick={() => removeRule(category.key, rule.key)}
                        >
                          <X size={14} aria-hidden="true" />
                        </Button>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            ))}
            {errors.namespaces && (
              <p role="alert" className="text-sm text-danger">
                {errors.namespaces}
              </p>
            )}
          </fieldset>

          {errors.form && (
            <p role="alert" className="text-sm text-danger">
              {errors.form}
            </p>
          )}

          <div className="mt-1 flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={update.isPending}>
              {update.isPending ? "Saving…" : "Save"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The registration's namespaces as rows the dialog can edit; anything malformed is skipped. */
function readNamespaces(raw: AppService["namespaces"]): Record<Category, RuleRow[]> {
  const source = (raw ?? {}) as AppserviceNamespaces;
  let key = 0;
  const read = (list: NamespaceRule[] | undefined): RuleRow[] =>
    (Array.isArray(list) ? list : [])
      .filter((rule) => rule && typeof rule.regex === "string")
      .map((rule) => ({ key: key++, regex: rule.regex, exclusive: Boolean(rule.exclusive) }));
  return { users: read(source.users), aliases: read(source.aliases), rooms: read(source.rooms) };
}

function writeNamespaces(rules: Record<Category, RuleRow[]>): AppserviceNamespaces {
  const write = (list: RuleRow[]): NamespaceRule[] =>
    list.map(({ regex, exclusive }) => ({ regex: regex.trim(), exclusive: Boolean(exclusive) }));
  return { users: write(rules.users), aliases: write(rules.aliases), rooms: write(rules.rooms) };
}
