import { useId, useState, type FormEvent } from "react";
import { useUpdateUser, type User, type UserUpdate } from "@/api/users";
import { ApiProblemError } from "@/api/problem";
import { getSession } from "@/lib/auth";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { Switch } from "@/components/ui/switch/Switch";
import { USER_TYPE_HINT, USER_TYPE_OPTIONS } from "./user-type";

type FieldName = "display_name" | "avatar_url" | "user_type" | "admin";

const FIELD_FOR_POINTER: Record<string, FieldName> = {
  "/display_name": "display_name",
  "/avatar_url": "avatar_url",
  "/user_type": "user_type",
  "/admin": "admin",
};

/**
 * Edits an account's own fields (`PATCH /users/{user_id}`): display name, avatar, kind of
 * account and whether they are a server administrator. Until now administrator was set only
 * when the account was made.
 *
 * Only the fields that changed are sent, so a field left alone is never touched (and a server
 * whose directory cannot change one field does not refuse a request about another). A refusal
 * is shown beside the field it names, in the server's words. The same profile fields are
 * edited in place in the page's "How they appear" section (`UserProfilePanel`).
 */
export function EditUserDialog({
  user,
  open,
  onOpenChange,
}: {
  user: User;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const update = useUpdateUser();
  const adminLabelId = useId();
  const adminHintId = useId();
  const [displayName, setDisplayName] = useState(user.display_name ?? "");
  const [avatarUrl, setAvatarUrl] = useState(user.avatar_url ?? "");
  const [userType, setUserType] = useState(user.user_type || "person");
  const [admin, setAdmin] = useState(Boolean(user.admin));
  const [errors, setErrors] = useState<Partial<Record<FieldName | "form", string>>>({});

  const self = getSession()?.operator.subject === user.user_id;
  const revokingOwnAdmin = self && Boolean(user.admin) && !admin;

  function patch(): UserUpdate {
    const body: UserUpdate = {};
    if (displayName !== (user.display_name ?? "")) body.display_name = displayName;
    if (avatarUrl !== (user.avatar_url ?? "")) body.avatar_url = avatarUrl;
    if (userType !== (user.user_type || "person"))
      body.user_type = userType === "person" ? null : userType;
    if (admin !== Boolean(user.admin)) body.admin = admin;
    return body;
  }

  function handleOpenChange(next: boolean) {
    if (!next) {
      setDisplayName(user.display_name ?? "");
      setAvatarUrl(user.avatar_url ?? "");
      setUserType(user.user_type || "person");
      setAdmin(Boolean(user.admin));
      setErrors({});
    }
    onOpenChange(next);
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setErrors({});
    const body = patch();
    if (Object.keys(body).length === 0) {
      handleOpenChange(false);
      return;
    }
    try {
      await update.mutateAsync({ userId: user.user_id, patch: body });
      onOpenChange(false);
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const { problem } = err;
        const next: Partial<Record<FieldName | "form", string>> = {};
        for (const item of problem.errors ?? []) {
          const field = item.pointer ? FIELD_FOR_POINTER[item.pointer] : undefined;
          const detail = item.detail ?? "refused";
          if (field) next[field] = `This server says: ${detail}.`;
          else next.form = detail;
        }
        if (Object.keys(next).length === 0)
          next.form = problem.detail ?? problem.title ?? "The server refused.";
        setErrors(next);
        return;
      }
      setErrors({ form: "Couldn’t reach the server." });
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title="Edit account"
        description={`How ${user.user_id} appears to other people, what kind of account it is, and whether it administers this server. Only what you change is sent.`}
      >
        <form className="flex flex-col gap-4" onSubmit={handleSubmit} noValidate>
          <Field
            label="Display name"
            hint="What other people see in rooms. Changing it updates their membership in every room they are in, as if they had changed it themself. Empty shows their user ID instead."
            error={errors.display_name}
          >
            {(fieldProps) => (
              <Input
                {...fieldProps}
                autoComplete="off"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
              />
            )}
          </Field>
          <Field
            label="Avatar"
            hint="The mxc:// address of an image already uploaded to this server. Empty removes the avatar."
            error={errors.avatar_url}
          >
            {(fieldProps) => (
              <Input
                {...fieldProps}
                autoComplete="off"
                spellCheck={false}
                className="font-identifier"
                placeholder="mxc://example.org/…"
                value={avatarUrl}
                onChange={(e) => setAvatarUrl(e.target.value)}
              />
            )}
          </Field>
          <Field label="Kind of account" hint={USER_TYPE_HINT} error={errors.user_type}>
            {(fieldProps) => (
              <Select
                {...fieldProps}
                options={USER_TYPE_OPTIONS}
                value={userType}
                onValueChange={setUserType}
              />
            )}
          </Field>
          <div className="flex items-start justify-between gap-4">
            <div>
              <p id={adminLabelId} className="text-sm font-medium text-text">
                Server administrator
              </p>
              <p id={adminHintId} className="text-sm text-text-muted">
                Can sign in to this interface and change anything on the server. Taking it away does
                not sign them out of their Matrix clients.
              </p>
              {errors.admin && (
                <p role="alert" className="mt-1 text-xs text-danger">
                  {errors.admin}
                </p>
              )}
            </div>
            <Switch
              checked={admin}
              onCheckedChange={setAdmin}
              aria-labelledby={adminLabelId}
              aria-describedby={adminHintId}
            />
          </div>
          {revokingOwnAdmin && (
            <p
              role="status"
              className="rounded-md border border-warning-border bg-warning-bg p-3 text-sm text-warning"
            >
              This is your own account. Without administrator you can no longer use this interface,
              and only another administrator can give it back.
            </p>
          )}

          {errors.form && (
            <p role="alert" className="text-sm text-danger">
              {errors.form}
            </p>
          )}

          <div className="mt-2 flex justify-end gap-2">
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
