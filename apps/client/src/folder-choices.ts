import type { NodeState } from "../../../packages/protocol-types";

export type FolderChoice = NodeState & { displayPath: string | null };
type FolderPage = { nodes: NodeState[]; next_after_id: string | null };

function folderPath(folder: NodeState, index: ReadonlyMap<string, NodeState>): string | null {
  const seen = new Set<string>(), parts: string[] = [];
  let current: NodeState | undefined = folder;
  while (current) {
    if (seen.has(current.node_id) || current.deleted || current.kind !== "folder") return null;
    seen.add(current.node_id);
    if (current.parent_id === null) return current.name === "" ? `/${parts.reverse().join("/")}` : null;
    parts.push(current.name);
    current = index.get(current.parent_id);
  }
  return null;
}

export async function loadFolderChoices(load: (after: string | null) => Promise<FolderPage>, isCurrent: () => boolean): Promise<FolderChoice[]> {
  const nodes = new Map<string, NodeState>(), cursors = new Set<string>();
  let after: string | null = null;
  for (let pageIndex = 0; pageIndex < 512; pageIndex++) {
    if (!isCurrent()) throw new Error("stale_folder_request");
    const page = await load(after);
    if (!isCurrent()) throw new Error("stale_folder_request");
    for (const node of page.nodes) {
      if (nodes.has(node.node_id)) throw new Error("duplicate_folder");
      nodes.set(node.node_id, node);
    }
    if (page.next_after_id === null) return [...nodes.values()]
      .filter((node) => node.kind === "folder" && !node.deleted)
      .map((node) => ({ ...node, displayPath: folderPath(node, nodes) }));
    if (!page.nodes.length || cursors.has(page.next_after_id)) throw new Error("invalid_folder_page");
    cursors.add(page.next_after_id); after = page.next_after_id;
  }
  throw new Error("too_many_folders");
}
