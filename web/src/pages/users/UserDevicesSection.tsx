import { useState, type FormEvent } from "react";
import { useUserDevices, useSignOutDevice, type Device } from "@/api/users";
import { useRenameDevice, useSignOutDevices } from "@/api/user-identity";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogTrigger, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { QueryProblemState } from "@/components/QueryProblemState";
import { MutationError } from "@/components/MutationError";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";

/**
 * A user's devices: rename one, sign one out, or pick several and sign them all out at once.
 * Signing a device out ends its sessions and removes its encryption keys, so nobody is handed
 * them again. What an administrator does about a lost phone, or a dozen stale browser sessions.
 */
export function UserDevicesSection({
  userId,
  canWrite,
  canModerate,
}: {
  userId: string;
  canWrite: boolean;
  canModerate: boolean;
}) {
  const { data: devices, isError, error, refetch } = useUserDevices(userId);
  const signOutDevice = useSignOutDevice();
  const signOutDevices = useSignOutDevices();
  const [selected, setSelected] = useState<string[]>([]);
  const [renaming, setRenaming] = useState<Device | null>(null);

  const items = devices?.items ?? [];
  const chosen = selected.filter((id) => items.some((d) => d.device_id === id));

  function toggle(deviceId: string, on: boolean) {
    setSelected((current) =>
      on ? [...current, deviceId] : current.filter((id) => id !== deviceId),
    );
  }

  return (
    <section aria-labelledby="devices-heading">
      <div className="mt-8 flex flex-wrap items-center justify-between gap-2">
        <h2 id="devices-heading" className="text-md font-medium text-text">
          Sessions
        </h2>
        {items.length > 0 && (
          <Dialog>
            <DialogTrigger asChild>
              <Button
                variant="secondary"
                size="sm"
                disabled={!canWrite || chosen.length === 0}
                title={!canWrite ? "Needs admin:write" : undefined}
              >
                Sign out selected{chosen.length > 0 ? ` (${chosen.length})` : ""}
              </Button>
            </DialogTrigger>
            <DialogContent
              title={`Sign out ${chosen.length} ${chosen.length === 1 ? "device" : "devices"}?`}
              description="Each one signs out immediately and its encryption keys are removed. The others stay signed in."
              footer={
                <>
                  <DialogClose asChild>
                    <Button variant="secondary">Cancel</Button>
                  </DialogClose>
                  <DialogClose asChild>
                    <Button
                      variant="danger"
                      onClick={() =>
                        signOutDevices.mutate(
                          { userId, deviceIds: chosen },
                          {
                            onSuccess: () => {
                              setSelected([]);
                              toast({
                                title: `Signed out ${chosen.length} ${chosen.length === 1 ? "device" : "devices"}`,
                              });
                            },
                          },
                        )
                      }
                    >
                      Sign out
                    </Button>
                  </DialogClose>
                </>
              }
            />
          </Dialog>
        )}
      </div>
      {signOutDevices.isError && (
        <MutationError
          error={signOutDevices.error}
          action="sign those devices out"
          className="mt-3"
        />
      )}
      {isError ? (
        <QueryProblemState
          error={error}
          resource="this user's sessions"
          onRetry={() => refetch()}
        />
      ) : items.length === 0 ? (
        <p className="mt-3 text-sm text-text-muted">No devices.</p>
      ) : (
        <ul className="mt-3 divide-y divide-border rounded-md border border-border">
          {items.map((d) => (
            <li key={d.device_id} className="flex items-center justify-between gap-3 px-4 py-3">
              <div className="flex items-center gap-3">
                <input
                  type="checkbox"
                  className="h-4 w-4"
                  aria-label={`Select ${d.display_name ?? d.device_id}`}
                  checked={chosen.includes(d.device_id)}
                  disabled={!canWrite}
                  onChange={(e) => toggle(d.device_id, e.target.checked)}
                />
                <div>
                  <p className="font-identifier text-text">{d.device_id}</p>
                  {d.display_name && <p className="text-xs text-text-muted">{d.display_name}</p>}
                </div>
              </div>
              <div className="flex items-center gap-3 text-xs text-text-muted">
                {d.last_seen_ip && <span className="font-identifier">{d.last_seen_ip}</span>}
                <RelativeTime at={d.last_seen_at} />
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={!canWrite}
                  aria-label={`Rename ${d.display_name ?? d.device_id}`}
                  onClick={() => setRenaming(d)}
                >
                  Rename
                </Button>
                <Dialog>
                  <DialogTrigger asChild>
                    <Button variant="ghost" size="sm" disabled={!canModerate}>
                      Sign out
                    </Button>
                  </DialogTrigger>
                  <DialogContent
                    title={`Sign out ${d.display_name ?? d.device_id}?`}
                    description="That device signs out immediately. The others stay signed in."
                    footer={
                      <>
                        <DialogClose asChild>
                          <Button variant="secondary">Cancel</Button>
                        </DialogClose>
                        <DialogClose asChild>
                          <Button
                            variant="danger"
                            onClick={() =>
                              signOutDevice.mutate(
                                { userId, deviceId: d.device_id },
                                { onSuccess: () => toast({ title: "Signed out" }) },
                              )
                            }
                          >
                            Sign out
                          </Button>
                        </DialogClose>
                      </>
                    }
                  />
                </Dialog>
              </div>
            </li>
          ))}
        </ul>
      )}
      {renaming && (
        <RenameDeviceDialog userId={userId} device={renaming} onClose={() => setRenaming(null)} />
      )}
    </section>
  );
}

function RenameDeviceDialog({
  userId,
  device,
  onClose,
}: {
  userId: string;
  device: Device;
  onClose: () => void;
}) {
  const rename = useRenameDevice();
  const [name, setName] = useState(device.display_name ?? "");

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    rename.mutate(
      { userId, deviceId: device.device_id, displayName: name.trim() === "" ? null : name.trim() },
      {
        onSuccess: () => {
          toast({ title: "Device renamed" });
          onClose();
        },
      },
    );
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        size="form"
        title={`Rename ${device.device_id}`}
        description="The name their own device list shows, and the one their contacts see beside its keys."
      >
        <form onSubmit={handleSubmit} className="flex flex-col gap-4" noValidate>
          <Field label="Device name" hint="Leave it empty to clear the name.">
            {(fieldProps) => (
              <Input
                {...fieldProps}
                value={name}
                maxLength={256}
                onChange={(e) => setName(e.target.value)}
              />
            )}
          </Field>
          {rename.isError && <MutationError error={rename.error} action="rename the device" />}
          <div className="mt-2 flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={rename.isPending}>
              {rename.isPending ? "Saving…" : "Save name"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
