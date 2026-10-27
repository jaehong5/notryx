import type { NodeState } from "../../../packages/protocol-types";

function hasControl(value: string): boolean {
  for (let i = 0; i < value.length; i++) {
    const code = value.charCodeAt(i);
    if (code < 32 || code === 127) return true;
  }
  return false;
}

export function resolveWorkspaceReference(
  nodes: readonly NodeState[], sourceId: string, reference: string,
): { node: NodeState; fragment: string | null } | null {
  if (!reference || reference.length > 4096 || hasControl(reference) || reference.includes("\\")) return null;
  const hash = reference.indexOf("#");
  const path = hash < 0 ? reference : reference.slice(0, hash);
  const rawFragment = hash < 0 ? null : reference.slice(hash + 1);
  if (path.startsWith("/") || path.includes("?") || /^[a-z][a-z0-9+.-]*:/iu.test(path)) return null;

  let fragment: string | null;
  let parts: string[];
  try {
    fragment = rawFragment === null ? null : decodeURIComponent(rawFragment);
    parts = path === "" ? [] : path.split("/").map((part) => decodeURIComponent(part));
  } catch {
    return null;
  }
  if (fragment !== null && hasControl(fragment)) return null;
  if (parts.some((part) => !part || hasControl(part) || part.includes("/") || part.includes("\\"))) return null;
  if (parts[0] && /^[a-z][a-z0-9+.-]*:/iu.test(parts[0])) return null;

  const byId = new Map<string, NodeState>();
  const children = new Map<string, NodeState[]>();
  for (const node of nodes) {
    if (byId.has(node.node_id)) return null;
    byId.set(node.node_id, node);
    if (node.parent_id !== null && !node.deleted) {
      const key = JSON.stringify([node.parent_id, node.name]);
      const matches = children.get(key) ?? [];
      matches.push(node);
      children.set(key, matches);
    }
  }
  const source = byId.get(sourceId);
  if (!source || source.deleted || source.kind !== "file" || source.parent_id === null) return null;

  const seen = new Set([source.node_id]);
  let parent = byId.get(source.parent_id);
  if (!parent) return null;
  while (true) {
    if (parent.deleted || parent.kind !== "folder" || seen.has(parent.node_id)) return null;
    seen.add(parent.node_id);
    if (parent.parent_id === null) {
      if (parent.name !== "") return null;
      break;
    }
    const next = byId.get(parent.parent_id);
    if (!next) return null;
    parent = next;
  }
  if (!parts.length) return { node: source, fragment };

  let current = byId.get(source.parent_id)!;
  for (const part of parts) {
    if (current.kind !== "folder") return null;
    if (part === ".") continue;
    if (part === "..") {
      if (current.parent_id === null) return null;
      current = byId.get(current.parent_id)!;
      continue;
    }
    const matches = children.get(JSON.stringify([current.node_id, part]));
    if (!matches || matches.length !== 1) return null;
    current = matches[0];
  }
  return current.kind === "file" && !current.deleted ? { node: current, fragment } : null;
}
