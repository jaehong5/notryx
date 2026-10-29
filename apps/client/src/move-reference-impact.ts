import type { NodeState } from "../../../packages/protocol-types";
import { classifyPreviewLink } from "./preview";
import type { PreviewReference } from "./preview-references";
import { resolveWorkspaceReference } from "./workspace-reference";

const MAX_REFERENCE_TREE_COMPARISONS = 200_000;
const MAX_REFERENCE_TREE_NODES = 10_000;
const MAX_REFERENCES = 2_000;
const MAX_REFERENCE_LENGTH = 4_096;

export type MoveReferenceDestination = { sourceId: string; parentId: string; name: string };
export type ReferenceResolution = { nodeId: string; fragment: string | null };
export type MoveReferenceResult = {
  reference: PreviewReference;
  outcome: "unchanged" | "target_changed" | "unresolved_before" | "unresolved_after" | "external";
  before: ReferenceResolution | null;
  after: ReferenceResolution | null;
};
export type MoveReferenceImpact =
  | { status: "checked"; results: MoveReferenceResult[] }
  | { status: "invalid_move" | "too_large"; results: [] };

export function compareMoveReferences(
  nodes: readonly NodeState[], destination: MoveReferenceDestination, references: readonly PreviewReference[],
): MoveReferenceImpact {
  if (nodes.length > MAX_REFERENCE_TREE_NODES || references.length > MAX_REFERENCES
    || nodes.length * references.length > MAX_REFERENCE_TREE_COMPARISONS
    || references.some((reference) => reference.target.length > MAX_REFERENCE_LENGTH)) {
    return { status: "too_large", results: [] };
  }
  const sources = nodes.filter((node) => node.node_id === destination.sourceId);
  const parents = nodes.filter((node) => node.node_id === destination.parentId);
  if (sources.length !== 1 || parents.length !== 1 || sources[0].deleted || sources[0].kind !== "file"
    || sources[0].parent_id === null || parents[0].deleted || parents[0].kind !== "folder"
    || destination.parentId === destination.sourceId || !destination.name
    || nodes.some((node) => node.node_id !== destination.sourceId && !node.deleted
      && node.parent_id === destination.parentId && node.name === destination.name)) {
    return { status: "invalid_move", results: [] };
  }
  const moved = nodes.map((node) => node.node_id === destination.sourceId
    ? { ...node, parent_id: destination.parentId, name: destination.name }
    : node);
  const results = references.map((reference): MoveReferenceResult => {
    const preview = classifyPreviewLink(reference.target);
    if (preview?.kind === "external") return { reference, outcome: "external", before: null, after: null };
    if (reference.kind === "image" && preview?.kind !== "document") {
      return { reference, outcome: "unresolved_before", before: null, after: null };
    }
    const original = preview ? resolveWorkspaceReference(nodes, destination.sourceId, reference.target) : null;
    const current = preview ? resolveWorkspaceReference(moved, destination.sourceId, reference.target) : null;
    const before = original && { nodeId: original.node.node_id, fragment: original.fragment };
    const after = current && { nodeId: current.node.node_id, fragment: current.fragment };
    if (!before) return { reference, outcome: "unresolved_before", before, after };
    if (!after) return { reference, outcome: "unresolved_after", before, after };
    return { reference, outcome: before.nodeId === after.nodeId ? "unchanged" : "target_changed", before, after };
  });
  return { status: "checked", results };
}
