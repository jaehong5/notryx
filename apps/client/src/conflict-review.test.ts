// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { ConflictReview, type ConflictReviewPorts, type ConflictReviewSnapshot } from "./conflict-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const bytes = (text: string) => new TextEncoder().encode(text);
const snapshot: ConflictReviewSnapshot = { binding, node_id:"document", operation_id:"operation", operation_seq:"1", generation:"2", base_revision:"base-rev", base_bytes:bytes("base\n"), base_hash:"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac", local_bytes:bytes("local 최신\n"), local_hash:"sha256:8f36d7c399b9111cd0ae5de633c61eb30af27f12c008d06e8584ad173fee4910", conflict_revision:"revision-at-412", remote_revision:"current-rev", remote_bytes:bytes("remote 현재\n"), remote_hash:"sha256:41295f40ed24c952eaaf3fd93b72afe6a00ff0e3cd7d0780fe42eae205ceabba" };
function mount(ports: ConflictReviewPorts, extra: Partial<Parameters<typeof ConflictReview>[0]> = {}) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  const props = { binding, documentId:"document", sessionId:"session", generation:"2", operationId:"operation", ports, onClose:vi.fn(), ...extra };
  return { container, root, props };
}
beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("기준·저장된 초안·조회 당시 서버 본문을 읽기 전용으로 구분한다", async () => {
  const ports: ConflictReviewPorts = { load:vi.fn(async()=>snapshot) }; const { container, root, props } = mount(ports);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="기준 원문"]')).not.toBeNull());
  expect(ports.load).toHaveBeenCalledExactlyOnceWith(binding, "document", "session", "2", "operation");
  const base = container.querySelector<HTMLTextAreaElement>('[aria-label="기준 원문"]')!;
  const local = container.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')!;
  const remote = container.querySelector<HTMLTextAreaElement>('[aria-label="조회 당시 서버 본문"]')!;
  expect(base.readOnly && local.readOnly && remote.readOnly).toBe(true);
  expect(base.value).toBe("base\n"); expect(local.value).toBe("local 최신\n"); expect(remote.value).toBe("remote 현재\n");
  expect(container.textContent).toContain("충돌이 발생한 뒤 달라졌을 수 있습니다");
  expect(container.textContent).toContain("저장된 초안");
  expect([...container.querySelectorAll("button")].map((item) => item.textContent)).toEqual(["닫기"]);
  await act(async () => { root.unmount(); });
});

test("비신뢰 본문을 텍스트로만 표시하고 HTML이나 Markdown을 실행하지 않는다", async () => {
  const hostile = '<img src=x onerror="alert(1)"><script>alert(1)</script>\n[link](javascript:alert(2))';
  const ports: ConflictReviewPorts = { load:vi.fn(async()=>({ ...snapshot, remote_bytes:bytes(hostile) })) };
  const { container, root, props } = mount(ports);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="조회 당시 서버 본문"]')).not.toBeNull());
  expect(container.querySelector<HTMLTextAreaElement>('[aria-label="조회 당시 서버 본문"]')!.value).toBe(hostile);
  expect(container.querySelector("script, img, a, iframe")).toBeNull();
  await act(async () => { root.unmount(); });
});

test("준비 오류나 비UTF-8 응답을 빈 비교 결과로 표시하지 않는다", async () => {
  for (const load of [async () => { throw new Error("remote_not_available"); }, async () => ({ ...snapshot, remote_bytes:new Uint8Array([0xff]) })]) {
    const ports: ConflictReviewPorts = { load:vi.fn(load) }; const { container, root, props } = mount(ports);
    await act(async () => { root.render(createElement(ConflictReview, props)); });
    await vi.waitFor(() => expect(container.textContent).toContain("비교 자료를 불러오지 못했습니다"));
    expect(container.querySelector('[aria-label="조회 당시 서버 본문"]')).toBeNull();
    expect(container.querySelector<HTMLButtonElement>('button')?.textContent).toBe("닫기");
    expect(props.onClose).not.toHaveBeenCalled();
    await act(async () => { root.unmount(); });
  }
});

test("다른 등록·문서·작업·generation의 응답은 표시하지 않는다", async () => {
  for (const wrong of [{ ...snapshot, binding:{ ...binding, workspace_epoch:"other" } }, { ...snapshot, node_id:"other" }, { ...snapshot, operation_id:"other" }, { ...snapshot, generation:"1" }]) {
    const ports: ConflictReviewPorts = { load:vi.fn(async()=>wrong) }; const { container, root, props } = mount(ports);
    await act(async () => { root.render(createElement(ConflictReview, props)); });
    await vi.waitFor(() => expect(container.textContent).toContain("비교 자료를 불러오지 못했습니다"));
    expect(container.querySelector('[aria-label="이 기기에 저장된 내 초안"]')).toBeNull();
    await act(async () => { root.unmount(); });
  }
});

test("generation 또는 등록이 바뀐 뒤 늦은 이전 응답은 새 비교 화면에 표시하지 않는다", async () => {
  let release!: (result: ConflictReviewSnapshot) => void;
  const ports: ConflictReviewPorts = { load:vi.fn().mockImplementationOnce(() => new Promise<ConflictReviewSnapshot>((resolve) => { release = resolve; })).mockResolvedValue({ ...snapshot, generation:"3", local_bytes:bytes("new draft") }) };
  const { container, root, props } = mount(ports);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await act(async () => { root.render(createElement(ConflictReview, { ...props, generation:"3" })); });
  await act(async () => { release(snapshot); });
  await vi.waitFor(() => expect(container.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe("new draft"));
  expect(container.textContent).not.toContain("local 최신");
  await act(async () => { root.unmount(); });
});

test("읽기 중 닫은 뒤 늦은 응답은 화면을 갱신하거나 원문을 변경하지 않는다", async () => {
  let release!: (result: ConflictReviewSnapshot) => void;
  const ports: ConflictReviewPorts = { load:vi.fn(() => new Promise<ConflictReviewSnapshot>((resolve) => { release = resolve; })) };
  const { container, root, props } = mount(ports);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await act(async () => { container.querySelector<HTMLButtonElement>("button")!.click(); });
  expect(props.onClose).toHaveBeenCalledOnce();
  await act(async () => { root.unmount(); release(snapshot); });
  expect(container.textContent).not.toContain("remote 현재");
  expect(ports.load).toHaveBeenCalledOnce();
});

test("본문의 CRLF·단독 CR·혼합 줄바꿈을 원문 기준으로 따로 표시한다", async () => {
  const ports: ConflictReviewPorts = { load:vi.fn(async()=>({ ...snapshot, base_bytes:bytes("one\r\ntwo\r\n"), local_bytes:bytes("one\rtwo\r"), remote_bytes:bytes("one\ntwo\r\nthree\r") })) };
  const { container, root, props } = mount(ports);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="기준 원문"]')).not.toBeNull());
  const base = container.querySelector('[aria-label="기준 원문"]')!.closest("label")!;
  const local = container.querySelector('[aria-label="이 기기에 저장된 내 초안"]')!.closest("label")!;
  const remote = container.querySelector('[aria-label="조회 당시 서버 본문"]')!.closest("label")!;
  expect(base.textContent).toContain("줄바꿈: CRLF");
  expect(local.textContent).toContain("줄바꿈: 단독 CR");
  expect(remote.textContent).toContain("줄바꿈: 혼합");
  await act(async () => { root.unmount(); });
});

test("조회 포트가 바뀌면 새 응답 전에는 이전 비교를 표시하지 않는다", async () => {
  const first: ConflictReviewPorts = { load:vi.fn(async()=>snapshot) };
  let release!: (result: ConflictReviewSnapshot) => void;
  const second: ConflictReviewPorts = { load:vi.fn(() => new Promise<ConflictReviewSnapshot>((resolve) => { release = resolve; })) };
  const { container, root, props } = mount(first);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="기준 원문"]')).not.toBeNull());
  await act(async () => { root.render(createElement(ConflictReview, { ...props, ports:second })); });
  expect(container.querySelector('[aria-label="기준 원문"]')).toBeNull();
  expect(container.textContent).toContain("불러오는 중");
  await act(async () => { release({ ...snapshot, local_bytes:bytes("fresh draft") }); });
  await vi.waitFor(() => expect(container.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe("fresh draft"));
  await act(async () => { root.unmount(); });
});
