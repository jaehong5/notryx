import { describe, expect, test } from "vitest";
import type { NodeState } from "../../../packages/protocol-types";
import { resolveWorkspaceReference } from "./workspace-reference";

function node(node_id: string, parent_id: string | null, name: string, kind: "file" | "folder"): NodeState {
  return {
    node_id, parent_id, name, kind, deleted: false,
    content_rev: kind === "file" ? "content" : null,
    content_etag: kind === "file" ? "etag" : null,
    content_hash: kind === "file" ? "hash" : null,
    size: kind === "file" ? "1" : null,
    meta_rev: "meta", meta_etag: "meta-etag", node_etag: "node-etag",
    children_generation: "0", last_change_seq: "0",
  };
}

const root = node("root", null, "", "folder");
const notes = node("notes", "root", "notes", "folder");
const source = node("source", "notes", "current.md", "file");
const sibling = node("sibling", "notes", "이름 있는 문서.md", "file");
const assets = node("assets", "root", "assets", "folder");
const image = node("image", "assets", "그림.png", "file");
const fixture = [root, notes, source, sibling, assets, image];

describe("workspace relative references", () => {
  test("resolves from the source document parent without rewriting names", () => {
    expect(resolveWorkspaceReference(fixture, "source", "./%EC%9D%B4%EB%A6%84%20%EC%9E%88%EB%8A%94%20%EB%AC%B8%EC%84%9C.md#part"))
      .toEqual({ node: sibling, fragment: "part" });
    expect(resolveWorkspaceReference(fixture, "source", "../assets/%EA%B7%B8%EB%A6%BC.png"))
      .toEqual({ node: image, fragment: null });
    expect(resolveWorkspaceReference(fixture, "source", "#heading"))
      .toEqual({ node: source, fragment: "heading" });
  });

  test("rejects URLs, absolute paths, root escapes and encoded separators", () => {
    for (const ref of ["https://example.invalid/track.png", "//example.invalid/x", "javascript:alert(1)",
      "/assets/그림.png", "../../secret", "%2e%2e/%2e%2e/secret", "..%2fassets/그림.png",
      "..%5cassets/그림.png", "../assets/%00.png", "../assets/%zz", "../assets/그림.png?x=1"]) {
      expect(resolveWorkspaceReference(fixture, "source", ref), ref).toBeNull();
    }
  });

  test("fails closed on missing and cyclic parent graphs or ambiguous names and IDs", () => {
    expect(resolveWorkspaceReference(fixture.filter((item) => item.node_id !== "root"), "source", "../assets/그림.png")).toBeNull();
    expect(resolveWorkspaceReference([root, { ...notes, parent_id: "source" }, source, sibling], "source", sibling.name)).toBeNull();
    expect(resolveWorkspaceReference([...fixture, node("duplicate", "notes", sibling.name, "file")], "source", sibling.name)).toBeNull();
    expect(resolveWorkspaceReference([...fixture, { ...source }], "source", sibling.name)).toBeNull();
    expect(resolveWorkspaceReference(fixture.map((item) => item.node_id === "assets" ? { ...item, deleted: true } : item), "source", "../assets/그림.png")).toBeNull();
    expect(resolveWorkspaceReference(fixture, "missing", sibling.name)).toBeNull();
  });

  test("decodes percent escapes only once and does not traverse a file", () => {
    const percent = node("percent", "notes", "%2e%2e.md", "file");
    expect(resolveWorkspaceReference([...fixture, percent], "source", "%252e%252e.md"))
      .toEqual({ node: percent, fragment: null });
    expect(resolveWorkspaceReference(fixture, "source", `${sibling.name}/child.md`)).toBeNull();
    expect(resolveWorkspaceReference(fixture, "source", "../assets")).toBeNull();
  });
});
