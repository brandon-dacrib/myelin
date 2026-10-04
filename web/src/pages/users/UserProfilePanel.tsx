import { useState, type FormEvent } from "react";
import { useUpdateUser, type User, type UserUpdate } from "@/api/users";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Field, Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { MutationError } from "@/components/MutationError";
import { toast } from "@/components/ui/toast/toast-store";
import { USER_TYPE_HINT, USER_TYPE_OPTIONS, userTypeValue, userTypeWire } from "./user-type";

type FieldName = "display_name" | "avatar_url" | "user_type";

const POINTERS: Record<string, FieldName> = {
  "/display_name": "display_name",
  "/avatar_url": "avatar_url",
  "/user_type": "user_type",
};

/**
 * How the account appears, edited in place (`PATCH /users/{user_id}`): display name, avatar and
 * kind of account.
 *
 * A name or avatar changed here is changed the way the person changing it in their own client
 * would: the server re-sends their membership in every room they are joined to, so the other
 * members, and other servers, see the new one. That is what the explanation says, because an
 * administrator renaming somebody is visible to everybody that person talks to.
 *
 * Only what changed is sent, so a field left alone is never touched.
 */
export function UserProfilePanel({ user, canWrite }: { user: User; canWrite: boolean }) {
  const update = useUpdateUser();
  const [displayName, setDisplayName] = useState(user.display_name ?? "");
  const [avatarUrl, setAvatarUrl] = useState(user.avatar_url ?? "");
  const [userType, setUserType] = useState(userTypeValue(user.user_type));
  const [errors, setErrors] = useState<Partial<Record<FieldName, string>>>({});

  const patch: UserUpdate = {};
  if (displayName.trim() !== (user.display_name ?? ""))
    patch.display_name = displayName.trim() || null;
  if (avatarUrl.trim() !== (user.avatar_url ?? "")) patch.avatar_url = avatarUrl.trim() || null;
  if (userType !== userTypeValue(user.user_type)) patch.user_type = userTypeWire(userType);
  const changed = Object.keys(patch).length > 0;
  const touchesRooms = "display_name" in patch || "avatar_url" in patch;

  function reset() {
    setDisplayName(user.display_name ?? "");
    setAvatarUrl(user.avatar_url ?? "");
    setUserType(userTypeValue(user.user_type));
    setErrors({});
    update.reset();
  }

  // `mutateAsync`, not `mutate` with callbacks: a save refetches the user, which re-keys this
  // panel (see `UserIdentitySection`), and `mutate`'s own callbacks never run once the
  // component that called it has gone. The promise still settles.
  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!changed) return;
    setErrors({});
    try {
      await update.mutateAsync({ userId: user.user_id, patch });
      toast({
        title: "Profile saved",
        description: touchesRooms
          ? "Their rooms are being updated with the new name and avatar."
          : undefined,
      });
    } catch (err) {
      if (!(err instanceof ApiProblemError)) return;
      const next: Partial<Record<FieldName, string>> = {};
      for (const item of err.problem.errors ?? []) {
        const field = item.pointer ? POINTERS[item.pointer] : undefined;
        if (field) next[field] = item.detail ?? "refused";
      }
      setErrors(next);
    }
  }

  const fieldErrorShown = Object.keys(errors).length > 0;

  return (
    <section aria-labelledby="profile-heading">
      <h2 id="profile-heading" className="mt-8 text-md font-medium text-text">
        How they appear
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        A name or avatar changed here changes for everybody, as if they had changed it in their own
        client: every room they are in gets an update to their membership, which the other members
        and other servers see.
      </p>
      {user.erased ? (
        <p className="mt-3 text-sm text-text-muted">
          Cleared when the account was erased; an erased account has no profile.
        </p>
      ) : (
        <form className="mt-3 flex flex-col gap-4" onSubmit={handleSubmit} noValidate>
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <Field
              label="Display name"
              hint="What other people see in rooms. Empty shows their user ID instead."
              error={errors.display_name}
            >
              {(fieldProps) => (
                <Input
                  {...fieldProps}
                  autoComplete="off"
                  disabled={!canWrite}
                  value={displayName}
                  onChange={(e) => setDisplayName(e.target.value)}
                />
              )}
            </Field>
            <Field
              label="Avatar URL"
              hint="The mxc:// address of an image already uploaded to this server. Empty removes it."
              error={errors.avatar_url}
            >
              {(fieldProps) => (
                <Input
                  {...fieldProps}
                  autoComplete="off"
                  spellCheck={false}
                  className="font-identifier"
                  placeholder="mxc://example.org/…"
                  disabled={!canWrite}
                  value={avatarUrl}
                  onChange={(e) => setAvatarUrl(e.target.value)}
                />
              )}
            </Field>
          </div>
          <Field label="Kind of account" hint={USER_TYPE_HINT} error={errors.user_type}>
            {(fieldProps) => (
              <Select
                {...fieldProps}
                disabled={!canWrite}
                options={USER_TYPE_OPTIONS}
                value={userType}
                onValueChange={setUserType}
              />
            )}
          </Field>
          {update.isError && !fieldErrorShown && (
            <MutationError error={update.error} action="save the profile" />
          )}
          {canWrite && (
            <div className="flex gap-2">
              <Button type="submit" variant="secondary" disabled={!changed || update.isPending}>
                {update.isPending ? "Saving…" : "Save profile"}
              </Button>
              {changed && (
                <Button type="button" variant="ghost" onClick={reset}>
                  Undo changes
                </Button>
              )}
            </div>
          )}
        </form>
      )}
    </section>
  );
}
