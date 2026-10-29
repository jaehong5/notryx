// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";
import { ConflictReview, type ConflictReviewSnapshot } from "./conflict-review";
import { ConflictCopyRetry } from "./conflict-copy-retry";
import { DeletedDraftCopyReview } from "./deleted-draft-copy-review";

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const bytes = new TextEncoder().encode("preserved draft");
const snapshot: ConflictReviewSnapshot = { binding, node_id: "document", operation_id: "operation", operation_seq: "1", generation: "1", base_revision: "base", base_bytes: bytes, base_hash: "base-hash", local_bytes: bytes, local_hash: "local-hash", conflict_revision: "conflict", remote_revision: "remote", remote_bytes: bytes, remote_hash: "remote-hash" };
const deleted = { binding, document_id: "document", edit_session_id: "session", generation: "1", local_hash: "local-hash", tombstone_seq: "2" };
function folder(id: string, parent: string | null, name: string): NodeState {
  return { node_id: id, parent_id: parent, name, kind: "folder", deleted: false, content_rev: null, content_etag: null, content_hash: null, size: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', children_generation: "0", last_change_seq: "1" };
}
const cases = ["conflict-copy", "conflict-copy-retry", "deleted-copy"] as const;
type Kind = typeof cases[number];
type Page = { nodes: NodeState[]; next_after_id: string | null };
async function mount(kind: Kind, loadFolders: (binding: WorkspaceRegistration, after: string | null) => Promise<Page>) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container), confirm = vi.fn(async () => {});
  await act(async () => {
    if (kind === "conflict-copy") root.render(createElement(ConflictReview, { binding, documentId: "document", sessionId: "session", generation: "1", operationId: "operation", ports: { load: async () => snapshot }, onClose: vi.fn(), onKeepCopy: confirm, loadFolders }));
    else if (kind === "conflict-copy-retry") root.render(createElement(ConflictCopyRetry, { binding, previousOperationId: "operation", previousParentId: "root", previousName: "old.md", loadFolders, busy: false, onConfirm: confirm, onCancel: vi.fn() }));
    else root.render(createElement(DeletedDraftCopyReview, { snapshot: deleted, localBytes: new Uint8Array(bytes), loadFolders, onConfirm: confirm, onCancel: vi.fn() }));
  });
  if (kind === "conflict-copy") await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-open"]')!.click(); });
  return { container, root, confirm };
}
function value(element: HTMLInputElement | HTMLSelectElement, text: string) {
  Object.getOwnPropertyDescriptor(element instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype, "value")!.set!.call(element, text);
  element.dispatchEvent(new Event(element instanceof HTMLSelectElement ? "change" : "input", { bubbles: true }));
}
beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

for (const kind of cases) {
  test(`${kind}: 별도 페이지의 조상을 읽어 같은 폴더 이름을 전체 경로로 구별하고 확인한다`, async () => {
    const load = vi.fn(async (_binding: WorkspaceRegistration, after: string | null): Promise<Page> => after === null
      ? { nodes: [folder("work-notes", "work", "notes"), folder("private-notes", "private", "notes")], next_after_id: "parents" }
      : { nodes: [folder("root", null, ""), folder("work", "root", "work"), folder("private", "root", "private")], next_after_id: null });
    const { container, root, confirm } = await mount(kind, load);
    try {
      const select = container.querySelector<HTMLSelectElement>(`[data-testid="${kind}-parent"]`)!;
      expect([...select.options].map((option) => option.textContent)).toContain("/work/notes");
      expect([...select.options].map((option) => option.textContent)).toContain("/private/notes");
      expect(load).toHaveBeenCalledWith(binding, "parents");
      await act(async () => { value(select, "private-notes"); value(container.querySelector<HTMLInputElement>(`[data-testid="${kind}-name"]`)!, "copy.md"); });
      await act(async () => { container.querySelector<HTMLButtonElement>(`[data-testid="${kind}-review"]`)!.click(); });
      expect(container.textContent).toContain("/private/notes 폴더에 copy.md");
      await act(async () => { container.querySelector<HTMLButtonElement>(`[data-testid="${kind}-confirm"]`)!.click(); });
      if (kind === "conflict-copy") expect(confirm).toHaveBeenCalledExactlyOnceWith(snapshot, "private-notes", "copy.md");
      else if (kind === "deleted-copy") expect(confirm).toHaveBeenCalledExactlyOnceWith(deleted, "private-notes", "copy.md");
      else expect(confirm).toHaveBeenCalledExactlyOnceWith("private-notes", "copy.md");
    } finally { await act(async () => root.unmount()); }
  });
  test(`${kind}: 조상 경로가 누락된 폴더를 저장 대상으로 확인하지 않는다`, async () => {
    const { container, root, confirm } = await mount(kind, async () => ({ nodes: [folder("orphan", "missing", "notes")], next_after_id: null }));
    try {
      const select = container.querySelector<HTMLSelectElement>(`[data-testid="${kind}-parent"]`)!;
      const option = select.querySelector<HTMLOptionElement>('[value="orphan"]')!;
      expect(option.disabled).toBe(true);
      expect(option.textContent).toContain("경로 확인 필요");
      await act(async () => { value(select, "orphan"); value(container.querySelector<HTMLInputElement>(`[data-testid="${kind}-name"]`)!, "copy.md"); });
      expect(container.querySelector<HTMLButtonElement>(`[data-testid="${kind}-review"]`)!.disabled).toBe(true);
      expect(confirm).not.toHaveBeenCalled();
    } finally { await act(async () => root.unmount()); }
  });
}
