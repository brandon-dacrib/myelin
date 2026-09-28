import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { TasksPage } from "./TasksPage";
import { TaskDetailPage } from "./TaskDetailPage";
import { validateTasksSearch } from "./tasks-search";

const ROUTES = [
  { path: "/tasks", component: TasksPage, validateSearch: validateTasksSearch },
  { path: "/tasks/$taskId", component: TaskDetailPage, validateSearch: validateTasksSearch },
];
const KNOWN = ["/users/$userId", "/rooms/$roomId", "/bridges/$bridgeId"];

function open(path: string) {
  return renderRoutes(ROUTES, path, KNOWN);
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Tasks", () => {
  it("lists tasks newest first, with a running one's progress", async () => {
    open("/tasks");
    const table = within(await screen.findByRole("table", { name: "Tasks" }));
    const rows = table.getAllByRole("row").slice(1);
    expect(rows).toHaveLength(6);
    expect(within(rows[0]).getByText("Purge remote media cache")).toBeInTheDocument();
    expect(within(rows[0]).getByRole("progressbar")).toHaveAccessibleName(/of 4,000 files/);
    expect(within(rows[2]).getByText("Failed")).toBeInTheDocument();
    expect(within(rows[5]).getByText("The server")).toBeInTheDocument();
    expect(
      within(rows[2]).getByRole("link", { name: "!whatsapp-portal-1:example.org" }),
    ).toBeVisible();
  });

  it("asks the server for the status and kind in the URL", async () => {
    let asked: URLSearchParams | undefined;
    server.use(
      http.get("/api/v1/tasks", ({ request }) => {
        asked = new URL(request.url).searchParams;
        return HttpResponse.json({ items: [], next_cursor: null, prev_cursor: null });
      }),
    );
    open("/tasks?status=failed&action=media.");
    expect(await screen.findByText("No matching tasks")).toBeInTheDocument();
    expect(asked?.get("status")).toBe("failed");
    expect(asked?.get("action")).toBe("media.");
  });

  it("does not list tasks without admin:read", async () => {
    await signIn(["moderation:read"]);
    open("/tasks");
    expect(await screen.findByText(/This needs the/)).toHaveTextContent("admin:read");
  });
});

describe("A task", () => {
  it("shows a running task's progress and cancels it after saying what that means", async () => {
    const user = userEvent.setup();
    open("/tasks/01J9ZT000000000000000000T6");
    expect(await screen.findByRole("heading", { name: "Purge remote media cache" })).toBeVisible();
    expect(screen.getByText("Deleting cached files")).toBeInTheDocument();
    expect(screen.getByRole("progressbar")).toHaveAttribute("aria-valuenow");

    await user.click(screen.getByRole("button", { name: "Cancel task" }));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent("nothing is rolled back");
    await user.click(within(dialog).getByRole("button", { name: "Cancel task" }));

    expect(await screen.findByText("Cancelled")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Cancel task" })).not.toBeInTheDocument();
  });

  it("says a scheduled task will not run if cancelled", async () => {
    const user = userEvent.setup();
    open("/tasks/01J9ZT000000000000000000T5");
    expect(await screen.findByText("Runs at")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Cancel task" }));
    expect(await screen.findByRole("dialog")).toHaveTextContent("It will not run.");
  });

  it("shows why a task failed and how far it got", async () => {
    open("/tasks/01J9ZT000000000000000000T4");
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "the room's owner replica stopped answering",
    );
    expect(screen.getByRole("progressbar")).toHaveAccessibleName("3 of 7 members when it stopped");
    expect(screen.getByText("40s")).toBeInTheDocument();
  });

  it("shows what a finished task reported", async () => {
    open("/tasks/01J9ZT000000000000000000T3");
    expect(await screen.findByRole("heading", { name: "Result" })).toBeVisible();
    expect(screen.getByText("Replayed")).toBeInTheDocument();
    expect(screen.getByText("42")).toBeInTheDocument();
    expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
  });

  it("offers no cancel to someone who can only read", async () => {
    await signIn(["admin:read"]);
    open("/tasks/01J9ZT000000000000000000T6");
    expect(await screen.findByRole("heading", { name: "Purge remote media cache" })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Cancel task" })).not.toBeInTheDocument();
  });
});
