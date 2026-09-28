import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import {
  useDeleteRoom,
  useJoinRoom,
  usePurgeRoomHistory,
  useRefreshRoom,
} from "@/api/room-contents";
import type { Room } from "@/api/rooms";
import { Button } from "@/components/ui/button/Button";
import { Field, Input, Textarea } from "@/components/ui/input/Input";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { MutationError } from "@/components/MutationError";
import { toast } from "@/components/ui/toast/toast-store";
import { localInputToRfc3339 } from "@/lib/rooms";
import { RoomTaskFollow } from "./RoomTaskFollow";

export type RoomAction = "join" | "purge" | "delete";

/** Whichever of the three room actions is open, or none. */
export function RoomActionDialog({
  action,
  room,
  onClose,
}: {
  action: RoomAction | null;
  room: Room;
  onClose: () => void;
}) {
  return (
    <Dialog open={action !== null} onOpenChange={(open) => !open && onClose()}>
      {action === "join" && <JoinUserContent room={room} onClose={onClose} />}
      {action === "purge" && <PurgeHistoryContent room={room} />}
      {action === "delete" && <DeleteRoomContent room={room} />}
    </Dialog>
  );
}

function roomLabel(room: Room): string {
  return room.name ?? room.canonical_alias ?? room.room_id;
}

function Checkbox({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string;
  hint?: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <label className="flex items-start gap-2 text-sm text-text">
      <input
        type="checkbox"
        className="mt-1 size-4 accent-[var(--color-accent)]"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span>
        {label}
        {hint && <span className="block text-xs text-text-muted">{hint}</span>}
      </span>
    </label>
  );
}

function JoinUserContent({ room, onClose }: { room: Room; onClose: () => void }) {
  const join = useJoinRoom();
  const [userId, setUserId] = useState("");
  return (
    <DialogContent
      size="form"
      title={`Join a user to ${roomLabel(room)}`}
      description="Makes a user on this server a member of the room. If its join rules would not let them in, a member who may invite invites them first."
      footer={
        <>
          <DialogClose asChild>
            <Button variant="secondary">Cancel</Button>
          </DialogClose>
          <Button
            disabled={!userId.trim() || join.isPending}
            onClick={() =>
              join.mutate(
                { roomId: room.room_id, userId: userId.trim() },
                {
                  onSuccess: (member) => {
                    toast({ title: `${member.user_id ?? userId} joined` });
                    onClose();
                  },
                },
              )
            }
          >
            Join user
          </Button>
        </>
      }
    >
      <Field label="User ID">
        {(field) => (
          <Input
            {...field}
            placeholder="@someone:your.server"
            value={userId}
            onChange={(e) => setUserId(e.target.value)}
          />
        )}
      </Field>
      {join.isError && <MutationError className="mt-3" error={join.error} action="join them" />}
    </DialogContent>
  );
}

function PurgeHistoryContent({ room }: { room: Room }) {
  const purge = usePurgeRoomHistory();
  const refresh = useRefreshRoom();
  const [before, setBefore] = useState("");
  const [deleteLocal, setDeleteLocal] = useState(false);
  const [taskId, setTaskId] = useState<string | null>(null);
  const rfc3339 = localInputToRfc3339(before);
  return (
    <DialogContent
      size="form"
      title={`Purge the history of ${roomLabel(room)}?`}
      description="Removes the room's messages sent before a moment from this server. State (name, members, permissions) is kept, and so is the newest event. Nobody on this server can read what is purged again."
      footer={
        taskId ? (
          <DialogClose asChild>
            <Button variant="secondary">Close</Button>
          </DialogClose>
        ) : (
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <Button
              variant="danger"
              disabled={!rfc3339 || purge.isPending}
              onClick={() =>
                rfc3339 &&
                purge.mutate(
                  { roomId: room.room_id, before: rfc3339, deleteLocalEvents: deleteLocal },
                  { onSuccess: (task) => setTaskId(task.id) },
                )
              }
            >
              Purge history
            </Button>
          </>
        )
      }
    >
      {taskId ? (
        <RoomTaskFollow taskId={taskId} onDone={() => refresh(room.room_id)} />
      ) : (
        <div className="flex flex-col gap-3">
          <Field label="Purge messages sent before">
            {(field) => (
              <Input
                {...field}
                type="datetime-local"
                step={1}
                value={before}
                onChange={(e) => setBefore(e.target.value)}
                className="w-60"
              />
            )}
          </Field>
          <Checkbox
            label="Also purge messages from this server's own users"
            hint="Off, only messages from other servers' users are purged, as Synapse does."
            checked={deleteLocal}
            onChange={setDeleteLocal}
          />
          {purge.isError && <MutationError error={purge.error} action="start the purge" />}
        </div>
      )}
    </DialogContent>
  );
}

function DeleteRoomContent({ room }: { room: Room }) {
  const del = useDeleteRoom();
  const navigate = useNavigate();
  const refresh = useRefreshRoom();
  const [block, setBlock] = useState(false);
  const [purge, setPurge] = useState(true);
  const [moveUsers, setMoveUsers] = useState(false);
  const [creator, setCreator] = useState("");
  const [newName, setNewName] = useState("");
  const [message, setMessage] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [taskId, setTaskId] = useState<string | null>(null);
  const expected = room.name ?? room.room_id;
  const confirmed = confirmation.trim() === expected || confirmation.trim() === room.room_id;

  return (
    <DialogContent
      size="form"
      title={`Delete ${roomLabel(room)}?`}
      description="Every member on this server leaves the room, its local aliases are removed and it leaves the directory. Other servers' users stay in their copies of it."
      footer={
        taskId ? (
          <DialogClose asChild>
            <Button variant="secondary">Close</Button>
          </DialogClose>
        ) : (
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <Button
              variant="danger"
              disabled={!confirmed || del.isPending || (moveUsers && !creator.trim())}
              onClick={() =>
                del.mutate(
                  {
                    roomId: room.room_id,
                    block,
                    purge,
                    message: moveUsers ? message : undefined,
                    newRoom: moveUsers
                      ? { creator: creator.trim(), name: newName.trim() || undefined }
                      : undefined,
                  },
                  { onSuccess: (task) => setTaskId(task.id) },
                )
              }
            >
              Delete room
            </Button>
          </>
        )
      }
    >
      {taskId ? (
        <RoomTaskFollow
          taskId={taskId}
          onDone={(status) => {
            refresh(room.room_id);
            if (status === "succeeded") {
              toast({ title: `${roomLabel(room)} was deleted` });
              navigate({ to: "/rooms", search: {} });
            }
          }}
        />
      ) : (
        <div className="flex flex-col gap-3">
          <Checkbox
            label="Purge it from this server"
            hint="Every event is removed and the room stops existing here; joining it answers not found."
            checked={purge}
            onChange={setPurge}
          />
          <Checkbox
            label="Block it, so nobody on this server can join it again"
            checked={block}
            onChange={setBlock}
          />
          <Checkbox
            label="Move its members to a new room first"
            checked={moveUsers}
            onChange={setMoveUsers}
          />
          {moveUsers && (
            <div className="flex flex-col gap-3 rounded-md border border-border p-3">
              <Field label="Created by (a user on this server)">
                {(field) => (
                  <Input
                    {...field}
                    placeholder="@moderator:your.server"
                    value={creator}
                    onChange={(e) => setCreator(e.target.value)}
                  />
                )}
              </Field>
              <Field label="New room name">
                {(field) => (
                  <Input {...field} value={newName} onChange={(e) => setNewName(e.target.value)} />
                )}
              </Field>
              <Field label="Message posted there">
                {(field) => (
                  <Textarea
                    {...field}
                    value={message}
                    onChange={(e) => setMessage(e.target.value)}
                  />
                )}
              </Field>
            </div>
          )}
          <Field
            label={
              <>
                Type <span className="font-identifier">{expected}</span> to confirm
              </>
            }
          >
            {(field) => (
              <Input
                {...field}
                value={confirmation}
                onChange={(e) => setConfirmation(e.target.value)}
              />
            )}
          </Field>
          {del.isError && <MutationError error={del.error} action="start the deletion" />}
        </div>
      )}
    </DialogContent>
  );
}
