import { expect, test } from "vitest";
import { WorkspaceController } from "./workspace-controller";
import type { WorkspaceSyncState } from "../../../packages/protocol-types";

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };

test("an obsolete connecting response cannot replace a newer ready response", async () => {
  const workspace = new WorkspaceController();
  let finish!: (state: WorkspaceSyncState) => void;
  const pending = workspace.readSync(() => new Promise((resolve) => { finish = resolve; }), () => true);
  await workspace.readSync(async () => ({ binding, status: "ready" }), () => true);
  finish({ binding, status: "connecting" });
  expect(await pending).toBeNull();
  expect(workspace.connecting).toBe(false);
});

test("a registration change rejects a pending status response", async () => {
  const workspace = new WorkspaceController();
  let finish!: (state: WorkspaceSyncState) => void;
  const pending = workspace.readSync(() => new Promise((resolve) => { finish = resolve; }), () => true);
  workspace.invalidate();
  finish({ binding, status: "connecting" });
  expect(await pending).toBeNull();
  expect(workspace.connecting).toBe(false);
});

test("a failed older inventory cannot replace the result of a newer inventory", async () => {
  const workspace = new WorkspaceController();
  let fail!: (error: Error) => void;
  const old = workspace.loadInventory(() => new Promise((_, reject) => { fail = reject; }), null);
  const rejected = expect(old).rejects.toThrow("stale_workspace_list");
  const fresh = await workspace.loadInventory(async () => ({ nodes: [], next_after_id: null }), null);
  fail(new Error("Network"));
  await rejected;
  expect(workspace.isCurrentInventory(fresh.revision)).toBe(true);
});

test.each(["newer status", "invalidated registration", "inactive caller"])("an obsolete status failure is ignored after %s", async (change) => {
  const workspace = new WorkspaceController();
  let fail!: (error: Error) => void;
  let current = true;
  const pending = workspace.readSync(() => new Promise((_, reject) => { fail = reject; }), () => current);
  const ignored = expect(pending).resolves.toBeNull();
  if (change === "newer status") await workspace.readSync(async () => ({ binding, status: "ready" }), () => true);
  else if (change === "invalidated registration") workspace.invalidate();
  else current = false;
  fail(new Error("obsolete failure"));
  await ignored;
  expect(workspace.connecting).toBe(false);
});

test("a current status failure is still reported", async () => {
  const workspace = new WorkspaceController();
  const failure = new Error("current failure");
  await expect(workspace.readSync(async () => { throw failure; }, () => true)).rejects.toBe(failure);
});
