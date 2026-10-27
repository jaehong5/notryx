import { expect, test } from "vitest";
import { WorkspaceListLoader, workspacePath, draftSelectionId } from "./workspace-list";
import { resolveWorkspaceReference } from "./workspace-reference";
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

test("loads 10,000 documents with their folders and workspace root", async () => {
  const loader = new WorkspaceListLoader();
  const count = 20_001;
  let pages = 0;
  const result = await loader.load(async (after) => {
    const start = after === null ? 0 : Number(after) + 1;
    const end = Math.min(start + 100, count);
    pages++;
    return {
      nodes: Array.from({ length: end - start }, (_, offset) => {
        const index = start + offset;
        return node(String(index).padStart(8, "0"), null, index ? `node-${index}` : "", index <= 10_000 ? "folder" : "file");
      }),
      next_after_id: end < count ? String(end - 1).padStart(8, "0") : null,
    };
  });
  expect(pages).toBe(201);
  expect(result.nodes).toHaveLength(count);
  expect(result.nodes.filter((item) => item.kind === "file")).toHaveLength(10_000);
  expect(result.nextAfterId).toBeNull();
});

test("continues past one bounded batch without losing or repeating nodes", async () => {
  const loader = new WorkspaceListLoader();
  const count = 55_601;
  const cursors: (string | null)[] = [];
  const read = async (after: string | null) => {
    cursors.push(after);
    const start = after === null ? 0 : Number(after) + 1;
    const end = Math.min(start + 100, count);
    return {
      nodes: Array.from({ length: end - start }, (_, offset) => node(String(start + offset).padStart(8, "0"), null, "file.md", "file")),
      next_after_id: end < count ? String(end - 1).padStart(8, "0") : null,
    };
  };
  const first = await loader.load(read);
  expect(first.nodes).toHaveLength(51_200);
  expect(first.nextAfterId).toBe("00051199");
  expect(cursors).toHaveLength(512);
  const second = await loader.load(read, first.nextAfterId);
  expect(second.nodes).toHaveLength(4_401);
  expect(second.nodes[0].node_id).toBe("00051200");
  expect(second.nodes.at(-1)?.node_id).toBe("00055600");
  expect(second.nextAfterId).toBeNull();
  expect(new Set([...first.nodes, ...second.nodes].map((item) => item.node_id)).size).toBe(count);
});

test("rejects a non-advancing pagination cursor without publishing partial results", async () => {
  const loader = new WorkspaceListLoader();
  let pages = 0;
  await expect(loader.load(async () => {
    pages++;
    return pages === 1
      ? { nodes: [node("a", null, "first", "file")], next_after_id: "a" }
      : { nodes: [], next_after_id: "a" };
  })).rejects.toThrow("문서 목록을 계속 읽을 수 없습니다.");
  expect(pages).toBe(2);
});

test.each([1, 100])("bounds inventory requests when every page has %i nodes", async (pageSize) => {
  const loader = new WorkspaceListLoader();
  let pages = 0;
  const result = await loader.load(async (after) => {
    const start = after === null ? 0 : Number(after) + 1;
    const end = start + pageSize;
    pages++;
    return {
      nodes: Array.from({ length: pageSize }, (_, offset) => node(String(start + offset).padStart(8, "0"), null, "file.md", "file")),
      next_after_id: String(end - 1).padStart(8, "0"),
    };
  });
  expect(pages).toBe(512);
  expect(result.nodes).toHaveLength(512 * pageSize);
  expect(result.nextAfterId).toBe(String(512 * pageSize - 1).padStart(8, "0"));
});

test("marks the exact batch boundary as complete when the last page is terminal", async () => {
  const loader = new WorkspaceListLoader();
  let pages = 0;
  const result = await loader.load(async (after) => {
    const start = after === null ? 0 : Number(after) + 1;
    const end = start + 100;
    pages++;
    return {
      nodes: Array.from({ length: 100 }, (_, offset) => node(String(start + offset).padStart(8, "0"), null, "file.md", "file")),
      next_after_id: end === 51_200 ? null : String(end - 1).padStart(8, "0"),
    };
  });
  expect(pages).toBe(512);
  expect(result.nodes).toHaveLength(51_200);
  expect(result.nextAfterId).toBeNull();
});

test("resolves paths and relative links across a 30,002-node inventory", async () => {
  const root = "00030001";
  const folder = "00030000";
  const source = "00000001";
  const target = "00029999";
  const count = 30_002;
  const result = await new WorkspaceListLoader().load(async (after) => {
    const start = after === null ? 0 : Number(after) + 1;
    const end = Math.min(start + 100, count);
    return {
      nodes: Array.from({ length: end - start }, (_, offset) => {
        const id = String(start + offset).padStart(8, "0");
        if (id === root) return node(id, null, "", "folder");
        if (id === folder) return node(id, root, "notes", "folder");
        return node(id, folder, id === source ? "source.md" : id === target ? "target.md" : `${id}.md`, "file");
      }),
      next_after_id: end < count ? String(end - 1).padStart(8, "0") : null,
    };
  });
  expect(result.nodes).toHaveLength(count);
  expect(result.nextAfterId).toBeNull();
  const index = new Map(result.nodes.map((item) => [item.node_id, item]));
  expect(workspacePath(index.get(source)!, index)).toBe("notes/source.md");
  expect(resolveWorkspaceReference(result.nodes, source, "target.md#part"))
    .toEqual({ node: index.get(target), fragment: "part" });
});

test("invalidating a continuation prevents publishing its response", async () => {
  const loader = new WorkspaceListLoader();
  let resolve!: (value: { nodes: NodeState[]; next_after_id: string | null }) => void;
  const pending = loader.load(() => new Promise((done) => { resolve = done; }), "a");
  loader.invalidate();
  resolve({ nodes: [node("b", null, "new", "file")], next_after_id: null });
  await expect(pending).rejects.toThrow("stale_workspace_list");
});

test.each([
  { nodes: [], next_after_id: "a" },
  { nodes: [node("a", null, "a", "file")], next_after_id: "b" },
  { nodes: [node("b", null, "b", "file"), node("a", null, "a", "file")], next_after_id: null },
])("rejects malformed or unordered page contents", async (page) => {
  await expect(new WorkspaceListLoader().load(async () => page)).rejects.toThrow();
});

test("rejects nodes at or before the requested continuation cursor", async () => {
  await expect(new WorkspaceListLoader().load(async () => ({ nodes: [node("a", null, "a", "file")], next_after_id: null }), "b"))
    .rejects.toThrow("문서 목록을 계속 읽을 수 없습니다.");
});

test("rejects an oversized page to keep the batch bounded", async () => {
  const nodes = Array.from({ length: 101 }, (_, index) => node(String(index).padStart(3, "0"), null, "file.md", "file"));
  await expect(new WorkspaceListLoader().load(async () => ({ nodes, next_after_id: null })))
    .rejects.toThrow("문서 목록을 계속 읽을 수 없습니다.");
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
