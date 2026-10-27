import type { NodeState, WorkspaceDraft } from "../../../packages/protocol-types";

type Page = { nodes: NodeState[]; next_after_id: string | null };

export function draftKey(draft: WorkspaceDraft): string {
  return `draft:${JSON.stringify([draft.binding.server_origin, draft.binding.workspace_id, draft.binding.workspace_epoch, draft.binding.device_id, draft.binding.device_stream_id, draft.document_id])}`;
}

export function draftSelectionId(draft: WorkspaceDraft, mode: "fixture" | "workspace"): string {
  return mode === "workspace" && draft.current_registration && draft.has_session ? draft.document_id : draftKey(draft);
}

export class WorkspaceListLoader {
  private revision = 0;

  invalidate(): void { this.revision++; }
  isCurrent(revision: number): boolean { return this.revision === revision; }

  async load(read: (after: string | null) => Promise<Page>): Promise<{ nodes: NodeState[]; revision: number }> {
    const revision = ++this.revision;
    const all: NodeState[] = [];
    const seen = new Set<string>();
    let after: string | null = null;
    for (let page = 0; page < 100; page++) {
      const result = await read(after);
      if (revision !== this.revision) throw new Error("stale_workspace_list");
      for (const node of result.nodes) {
        if (seen.has(node.node_id)) throw new Error("문서 목록이 중복되었습니다. 다시 시도해 주세요.");
        seen.add(node.node_id);
        all.push(node);
      }
      if (result.next_after_id === null) return { nodes: all, revision };
      if (result.next_after_id === after) throw new Error("문서 목록을 계속 읽을 수 없습니다.");
      after = result.next_after_id;
    }
    throw new Error("문서 목록이 한도를 초과했습니다.");
  }
}

export function workspacePath(node: NodeState, index: Map<string, NodeState>): string {
  const parts = [node.name];
  const seen = new Set([node.node_id]);
  let parent = node.parent_id;
  while (parent !== null) {
    const item = index.get(parent);
    if (!item || seen.has(parent)) return `[경로 확인 필요]/${parts.join("/")}`;
    seen.add(parent);
    if (item.name) parts.unshift(item.name);
    parent = item.parent_id;
  }
  return parts.join("/");
}
