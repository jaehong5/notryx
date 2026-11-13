// @vitest-environment jsdom
import { beforeEach, expect, test, vi } from "vitest";

const render = vi.hoisted(() => vi.fn());
vi.mock("./mermaid-render", () => ({ renderMermaid: render }));

beforeEach(() => { vi.resetModules(); render.mockReset(); });

test("returns a render result for native polling without invoking an app bridge", async () => {
  render.mockResolvedValue({ svg: "<svg>한글</svg>", error: null });
  await import("./mermaid-android-renderer");
  expect(window.__NOTRYX_ANDROID_MERMAID_RESULT__).toBeNull();
  window.__NOTRYX_ANDROID_MERMAID_START__({ source: "flowchart LR\nA-->B", theme: "dark" });
  await vi.waitFor(() => expect(window.__NOTRYX_ANDROID_MERMAID_RESULT__).toEqual({ svg: "<svg>한글</svg>", error: null }));
  window.__NOTRYX_ANDROID_MERMAID_START__({ source: "other", theme: "light" });
  expect(render).toHaveBeenCalledTimes(1);
  expect(render).toHaveBeenCalledWith("flowchart LR\nA-->B", "dark");
});

test.each([null, { source: 42, theme: "light" }, { source: "graph TD", theme: "external" }])(
  "rejects malformed native input before rendering", async (request) => {
    await import("./mermaid-android-renderer");
    window.__NOTRYX_ANDROID_MERMAID_START__(request);
    expect(render).not.toHaveBeenCalled();
    expect(window.__NOTRYX_ANDROID_MERMAID_RESULT__).toEqual({ svg: null, error: "mermaid_invalid_request" });
  },
);

test("preserves a source fallback error without writing document HTML", async () => {
  render.mockResolvedValue({ svg: null, error: "mermaid_document_config" });
  await import("./mermaid-android-renderer");
  const before = document.body.innerHTML;
  window.__NOTRYX_ANDROID_MERMAID_START__({ source: "<script>bad</script>", theme: "light" });
  await vi.waitFor(() => expect(window.__NOTRYX_ANDROID_MERMAID_RESULT__?.error).toBe("mermaid_document_config"));
  expect(document.body.innerHTML).toBe(before);
});
