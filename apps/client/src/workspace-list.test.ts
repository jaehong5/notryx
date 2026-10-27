import { expect, test } from "vitest";
import { WorkspaceListLoader, workspacePath, draftSelectionId } from "./workspace-list";
import type { NodeState, WorkspaceDraft } from "../../../packages/protocol-types";

const node = (node_id: string, parent_id: string | null, name: string, kind: "file" | "folder"): NodeState => ({
  node_id, parent_id, name, kind, deleted: false, content_rev: null, content_etag: null,
  meta_rev: "r", meta_etag: "e", node_etag: "n", content_hash: null, size: null,
  children_generation: "0", last_change_seq: "0",
});

test("a new registration invalidates an entire older paginated listing", async () => {
  const loader = new WorkspaceListLoader();
  let resolve!: (value: { nodes: NodeState[]; next_after_id: string | null }) => void;
  const oldPage = new Promise<{ nodes: NodeState[]; next_after_id: string | null }>((done) => { resolve = done; });
  const oldRead = loader.load(async () => oldPage);
  const fresh = await loader.load(async () => ({ nodes: [node("b", null, "new", "file")], next_after_id: null }));
  resolve({ nodes: [node("a", null, "old", "file")], next_after_id: "a" });
  await expect(oldRead).rejects.toThrow("stale_workspace_list");
  expect(fresh.nodes.map((item) => item.name)).toEqual(["new"]);
});

test("paths distinguish same names and terminate on missing or cyclic parents", () => {
  const entries = [node("root", null, "", "folder"), node("a", "root", "A", "folder"), node("b", "root", "B", "folder"),
    node("one", "a", "README.md", "file"), node("two", "b", "README.md", "file"),
    node("missing", "absent", "lost.md", "file"), node("cycle", "cycle", "loop.md", "file")];
  const index = new Map(entries.map((item) => [item.node_id, item]));
  expect(workspacePath(entries[3], index)).toBe("A/README.md");
  expect(workspacePath(entries[4], index)).toBe("B/README.md");
  expect(workspacePath(entries[5], index)).toContain("lost.md");
  expect(workspacePath(entries[6], index)).toContain("loop.md");
});

test("the caller can reject a listing invalidated after its final page", async () => {
  const loader = new WorkspaceListLoader();
  const result = await loader.load(async () => ({ nodes: [], next_after_id: null }));
  loader.invalidate();
  expect(loader.isCurrent(result.revision)).toBe(false);
});

test("only a draft from the active session shares the remote document identity", () => {
  const draft: WorkspaceDraft = {
    binding: { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" },
    document_id: "document", generation: "1", current_registration: true, has_session: true,
  };
  expect(draftSelectionId(draft, "workspace")).toBe("document");
  expect(draftSelectionId({ ...draft, has_session: false }, "workspace")).not.toBe("document");
  expect(draftSelectionId({ ...draft, current_registration: false }, "workspace")).not.toBe("document");
  expect(draftSelectionId(draft, "fixture")).not.toBe("document");
});
