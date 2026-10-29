import { describe, expect, test } from "vitest";
import { collectPreviewReferences, PREVIEW_REFERENCE_SOURCE_LIMIT } from "./preview-references";

describe("미리보기에서 렌더링하는 링크 수집", () => {
  test("일반 링크와 이미지, 참조형 링크를 사용 순서대로 수집한다", () => {
    const source = "[one](sibling.md#part) ![image](../assets/picture.png) [two][reference] ![three][reference]\n\n[reference]: target.md\n";
    expect(collectPreviewReferences(source)).toEqual({
      status: "checked",
      references: [
        { kind: "link", target: "sibling.md#part" },
        { kind: "image", target: "../assets/picture.png" },
        { kind: "link", target: "target.md" },
        { kind: "image", target: "target.md" },
      ],
    });
  });

  test("미리보기와 같은 참조 정규화와 최초 정의를 사용한다", () => {
    const source = "[FOO bar][] [foo BAR] [x][foo bar]\n\n[foo bar]: first.md\n[FOO   BAR]: second.md\n";
    expect(collectPreviewReferences(source)).toEqual({
      status: "checked",
      references: [
        { kind: "link", target: "first.md" },
        { kind: "link", target: "first.md" },
        { kind: "link", target: "first.md" },
      ],
    });
  });

  test("URL의 entity, 괄호 escape와 공백은 미리보기의 해석대로 수집한다", () => {
    const source = "[a](foo&amp;bar.md) [b](foo\\(bar\\).md) [c](<한 글.md>)";
    expect(collectPreviewReferences(source)).toEqual({
      status: "checked",
      references: [
        { kind: "link", target: "foo&bar.md" },
        { kind: "link", target: "foo(bar).md" },
        { kind: "link", target: "%ED%95%9C%20%EA%B8%80.md" },
      ],
    });
  });

  test("코드, 미정의 참조와 생 HTML 안의 문자열을 렌더링 링크로 세지 않는다", () => {
    const source = "`[inline](bad.md)`\n\n```md\n[fenced](bad.md)\n```\n\n[missing][not-defined]\n\n<a href=\"hidden.md\">raw</a>\n\n<!-- [comment](bad.md) -->\n";
    expect(collectPreviewReferences(source)).toEqual({ status: "checked", references: [] });
  });

  test("외부 주소와 fragment를 보존하여 후속 경로 판정에 넘긴다", () => {
    const source = "[external](https://example.invalid/x) [anchor](#heading) [query](target.md?x=1)";
    expect(collectPreviewReferences(source)).toEqual({
      status: "checked",
      references: [
        { kind: "link", target: "https://example.invalid/x" },
        { kind: "link", target: "#heading" },
        { kind: "link", target: "target.md?x=1" },
      ],
    });
  });

  test("각주에서 미리보기가 생성하는 참조를 제외하고 본문의 링크만 수집한다", () => {
    const source = "note[^x]\n\n[^x]: [other](other.md)";
    expect(collectPreviewReferences(source)).toEqual({
      status: "checked",
      references: [{ kind: "link", target: "other.md" }],
    });
  });

  test("검사 크기를 넘은 본문은 빈 결과의 성공으로 표시하지 않는다", () => {
    const source = "a".repeat(PREVIEW_REFERENCE_SOURCE_LIMIT + 1);
    expect(collectPreviewReferences(source)).toEqual({ status: "too_large", references: [] });
  });
});
