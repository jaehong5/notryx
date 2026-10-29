import { describe, expect, test } from "vitest";
import type { NodeState } from "../../../packages/protocol-types";
import type { PreviewReference } from "./preview-references";
import { compareMoveReferences } from "./move-reference-impact";

function node(id: string, parent: string | null, name: string, kind: "file" | "folder"): NodeState {
  return {
    node_id: id, parent_id: parent, name, kind, deleted: false,
    content_rev: kind === "file" ? "content" : null,
    content_etag: kind === "file" ? "etag" : null,
    content_hash: kind === "file" ? "hash" : null,
    size: kind === "file" ? "1" : null,
    meta_rev: "meta", meta_etag: "meta-etag", node_etag: "node-etag",
    children_generation: "0", last_change_seq: "0",
  };
}

const root = node("root", null, "", "folder");
const before = node("before", "root", "before", "folder");
const after = node("after", "root", "after", "folder");
const source = node("source", "before", "source.md", "file");
const oldSibling = node("old-sibling", "before", "sibling.md", "file");
const newSibling = node("new-sibling", "after", "sibling.md", "file");
const oldOnly = node("old-only", "before", "old.md", "file");
const shared = node("shared", "root", "image.png", "file");
const fixture = [root, before, after, source, oldSibling, newSibling, oldOnly, shared];
const target = { sourceId: "source", parentId: "after", name: "renamed.md" };

const link = (target: string): PreviewReference => ({ kind: "link", target });
const image = (target: string): PreviewReference => ({ kind: "image", target });

describe("가상 이동의 상대 링크 영향 판정", () => {
  test("이동 전후 대상 ID와 fragment를 비교하고 원문 트리를 변경하지 않는다", () => {
    const tree = structuredClone(fixture);
    const references = [link("sibling.md#part"), image("../image.png"), link("old.md"), link("#heading")];
    const originalTree = structuredClone(tree);
    const originalReferences = structuredClone(references);
    expect(compareMoveReferences(tree, target, references)).toEqual({
      status: "checked",
      results: [
        { reference: references[0], outcome: "target_changed", before: { nodeId: "old-sibling", fragment: "part" }, after: { nodeId: "new-sibling", fragment: "part" } },
        { reference: references[1], outcome: "unchanged", before: { nodeId: "shared", fragment: null }, after: { nodeId: "shared", fragment: null } },
        { reference: references[2], outcome: "unresolved_after", before: { nodeId: "old-only", fragment: null }, after: null },
        { reference: references[3], outcome: "unchanged", before: { nodeId: "source", fragment: "heading" }, after: { nodeId: "source", fragment: "heading" } },
      ],
    });
    expect(tree).toEqual(originalTree);
    expect(references).toEqual(originalReferences);
  });

  test("이름 변경에 따른 자기 이름 참조와 UTF-8 및 퍼센트 인코딩을 판정한다", () => {
    const unicode = node("unicode", "before", "한 글.md", "file");
    const percent = node("percent", "before", "%2e%2e.md", "file");
    const references = [link("source.md"), link("%ED%95%9C%20%EA%B8%80.md#%ED%95%9C"), link("%252e%252e.md")];
    const result = compareMoveReferences([...fixture, unicode, percent], { ...target, parentId: "before" }, references);
    expect(result.status).toBe("checked");
    if (result.status !== "checked") throw new Error("expected checked result");
    expect(result.results.map((item) => item.outcome)).toEqual(["unresolved_after", "unchanged", "unchanged"]);
    expect(result.results[1].before).toEqual({ nodeId: "unicode", fragment: "한" });
    expect(result.results[1].after).toEqual({ nodeId: "unicode", fragment: "한" });
    expect(result.results[2].before?.nodeId).toBe("percent");
  });

  test("기존 미해결과 이동 후 미해결을 구분하고 위험 경로를 해석하지 않는다", () => {
    const references = [link("missing.md"), link("../..%2foutside.md"), link("%zz"), link("target.md?x=1"), link("../../outside.md"), link("old.md")];
    const result = compareMoveReferences(fixture, target, references);
    expect(result.status).toBe("checked");
    if (result.status !== "checked") throw new Error("expected checked result");
    expect(result.results.map((item) => item.outcome)).toEqual([
      "unresolved_before", "unresolved_before", "unresolved_before", "unresolved_before", "unresolved_before", "unresolved_after",
    ]);
  });

  test("외부 주소는 상대 경로 영향 검사에서 제외한다", () => {
    const ref = link("https://example.invalid/page");
    expect(compareMoveReferences(fixture, target, [ref])).toEqual({
      status: "checked", results: [{ reference: ref, outcome: "external", before: null, after: null }],
    });
  });

  test("불완전하거나 모호한 트리에서 해석 실패를 영향 없음으로 세지 않는다", () => {
    for (const tree of [
      fixture.filter((item) => item.node_id !== "root"),
      [...fixture, node("duplicate", "before", "sibling.md", "file")],
      [...fixture, { ...source }],
      fixture.map((item) => item.node_id === "before" ? { ...item, parent_id: "source" } : item),
    ]) {
      const result = compareMoveReferences(tree, target, [link("sibling.md")]);
      if (result.status === "checked") expect(result.results[0].outcome).toBe("unresolved_before");
      else expect(result.status).toBe("invalid_move");
    }
  });

  test("원본이나 목적지 폴더가 없거나 삭제되면 검사를 수행하지 않는다", () => {
    for (const tree of [
      fixture.filter((item) => item.node_id !== "source"),
      fixture.filter((item) => item.node_id !== "after"),
      fixture.map((item) => item.node_id === "after" ? { ...item, deleted: true } : item),
      fixture.map((item) => item.node_id === "after" ? { ...item, kind: "file" as const } : item),
    ]) expect(compareMoveReferences(tree, target, [link("sibling.md")])).toEqual({ status: "invalid_move", results: [] });
  });

  test("트리와 링크의 비교 비용이 큰 경우 전부 미검사로 반환한다", () => {
    const tree = [...fixture, ...Array.from({ length: 1000 }, (_, index) => node(`extra-${index}`, "root", `extra-${index}.md`, "file"))];
    const references = Array.from({ length: 200 }, () => link("sibling.md"));
    expect(compareMoveReferences(tree, target, references)).toEqual({ status: "too_large", results: [] });
  });

  test("링크가 없어도 과도하게 큰 트리는 미검사로 반환한다", () => {
    const tree = [...fixture, ...Array.from({ length: 10_000 }, (_, index) => node(`extra-${index}`, "root", `extra-${index}.md`, "file"))];
    expect(compareMoveReferences(tree, target, [])).toEqual({ status: "too_large", results: [] });
  });

  test("개별 주소가 해석 길이 상한을 넘으면 미검사로 반환한다", () => {
    expect(compareMoveReferences(fixture, target, [link("a".repeat(4097))])).toEqual({ status: "too_large", results: [] });
  });

  test("이미지에서 fragment만 지정한 경우 미리보기가 로드하지 않으므로 미해석으로 판정한다", () => {
    const result = compareMoveReferences(fixture, target, [image("#heading")]);
    expect(result.status).toBe("checked");
    if (result.status !== "checked") throw new Error("expected checked result");
    expect(result.results[0].outcome).toBe("unresolved_before");
    expect(result.results[0].before).toBeNull();
  });

  test("목적지에 동일한 이름의 다른 문서가 있으면 가상 이동을 검사하지 않는다", () => {
    const collision = node("collision", "after", "renamed.md", "file");
    expect(compareMoveReferences([...fixture, collision], target, [link("sibling.md")])).toEqual({ status: "invalid_move", results: [] });
  });
});
