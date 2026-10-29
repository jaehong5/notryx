// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { ConflictReview, type ConflictReviewPorts, type ConflictReviewSnapshot } from "./conflict-review";
import { openSource, saveSource } from "./source";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const bytes = (text: string) => new TextEncoder().encode(text);
const snapshot: ConflictReviewSnapshot = {
  binding, node_id: "document", operation_id: "operation", operation_seq: "1", generation: "2",
  base_revision: "base-rev", base_bytes: bytes("base\r\n"), base_hash: "sha256:f8ba93752c42985276cab7de1169609631ed696b04999d05d49d93169ec05c1a",
  local_bytes: bytes("\uFEFFlocal\r\n초안\r"), local_hash: "sha256:a4965a066b0fccd3d4b29663a218f0c31315fbe7be0ec2f04be44154b39d653b",
  conflict_revision: "revision-at-412", remote_revision: "current-rev", remote_bytes: bytes("remote\n"), remote_hash: "sha256:c81662749eb30a2c7f6b6b9b1a561a7b0be5cc115759b72442ddb476e7712751",
};

function mount(value: ConflictReviewSnapshot = snapshot) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  const save = vi.fn<(shown: ConflictReviewSnapshot) => Promise<void>>().mockResolvedValue(undefined);
  const ports: ConflictReviewPorts = { load: vi.fn(async () => value) };
  const props = { binding, documentId: "document", sessionId: "session", generation: "2", operationId: "operation", ports, onClose: vi.fn(), onSaveMerged: save };
  return { container, root, save, ports, props };
}

async function click(container: HTMLElement, selector: string) {
  const button = container.querySelector<HTMLButtonElement>(selector);
  expect(button).not.toBeNull();
  expect(button!.disabled).toBe(false);
  await act(async () => { button!.click(); });
}

beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("기존 편집기에서 만든 기기 초안을 비교하고 별도 확인한 뒤에만 합본으로 선택한다", async () => {
  const selected = { ...snapshot, local_bytes: bytes("\uFEFF합친 내용\n둘째 줄\n"), local_hash:"sha256:93b7aa40e944b625d877cf0ff92408f82711110728edbe076f1ce7a1493ce74a" };
  const { container, root, save, props } = mount(selected);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="이 기기에 저장된 내 초안"]')).not.toBeNull());
  expect(container.textContent).toMatch(/편집기|초안/);
  expect(container.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')!.value).toBe("합친 내용\n둘째 줄\n");
  expect(container.querySelector('[aria-label="합본 편집"]')).toBeNull();
  await click(container, '[data-testid="conflict-merge-review"]');
  expect(save).not.toHaveBeenCalled();
  expect(container.textContent).toContain("확인한 서버 본문이 바뀌면 다시 비교해야 합니다.");
  expect(container.querySelector<HTMLTextAreaElement>('[aria-label="조회 당시 서버 본문"]')?.value).toBe("remote\n");
  await click(container, '[data-testid="conflict-merge-confirm"]');
  expect(save).toHaveBeenCalledExactlyOnceWith(selected);
  expect(Array.from(save.mock.calls[0][0].local_bytes)).toEqual(Array.from(selected.local_bytes));
  await act(async () => { root.unmount(); });
});

test("CRLF 또는 단독 CR 초안은 실제 편집 전 합본으로 전송하지 않는다", async () => {
  const { container, root, save, props } = mount();
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="이 기기에 저장된 내 초안"]')).not.toBeNull());
  const review = container.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-review"]');
  expect(review).not.toBeNull();
  await act(async () => { review!.click(); });
  expect(save).not.toHaveBeenCalled();
  expect(container.querySelector('[data-testid="conflict-merge-confirm"]')).toBeNull();
  expect(container.textContent).toMatch(/편집|LF/);
  await act(async () => { root.unmount(); });
});

test("실제 편집 후 undo한 초안도 BOM과 LF를 보존하여 그대로 확인한다", async () => {
  const original = bytes("\uFEFFlocal\r\n초안\r");
  const source = openSource(original);
  expect(Array.from(saveSource(source, source.text))).toEqual(Array.from(original));
  const editedAndUndone = saveSource(source, source.text, true);
  expect(Array.from(editedAndUndone)).toEqual(Array.from(bytes("\uFEFFlocal\n초안\n")));
  const selected = { ...snapshot, local_bytes: editedAndUndone, local_hash:"sha256:883a0788a74883bbce1820fcd09076e1d027b097ad319eb53ead7e39c82111d3" };
  const { container, root, save, props } = mount(selected);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="이 기기에 저장된 내 초안"]')).not.toBeNull());
  await click(container, '[data-testid="conflict-merge-review"]');
  await click(container, '[data-testid="conflict-merge-confirm"]');
  expect(save).toHaveBeenCalledExactlyOnceWith(selected);
  expect(Array.from(save.mock.calls[0][0].local_bytes)).toEqual(Array.from(editedAndUndone));
  await act(async () => { root.unmount(); });
});

test("BOM을 포함한 5 MiB checkpoint를 본문 변경 없이 그대로 선택한다", async () => {
  const original = bytes("\uFEFF" + "x".repeat(5 * 1024 * 1024 - 3));
  const selected = { ...snapshot, local_bytes: original, local_hash:"sha256:c2fe0be1bc6b3c281b12b5373ba28e84baca29f6557a99115d5bc1d5a4b08918" };
  const { container, root, save, props } = mount(selected);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="이 기기에 저장된 내 초안"]')).not.toBeNull());
  await click(container, '[data-testid="conflict-merge-review"]');
  await click(container, '[data-testid="conflict-merge-confirm"]');
  expect(save).toHaveBeenCalledOnce();
  const actual = save.mock.calls[0][0].local_bytes;
  expect(actual.byteLength).toBe(5 * 1024 * 1024);
  expect(actual.every((value, index) => value === original[index])).toBe(true);
  await act(async () => { root.unmount(); });
});

test("5 MiB보다 큰 기기 초안은 합본으로 선택하지 않는다", async () => {
  const selected = { ...snapshot, local_bytes: bytes("\uFEFF" + "x".repeat(5 * 1024 * 1024 - 2)), local_hash:"sha256:31b0a5ba116bcb2048f4b4e77592cb798804e0cfdafe3e17c05812d70b276cec" };
  const { container, root, save, props } = mount(selected);
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("비교 자료를 불러오지 못했습니다"));
  expect(container.querySelector('[data-testid="conflict-merge-review"]')).toBeNull();
  expect(save).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("비교 snapshot이 바뀌면 이전 합본 확인을 새 문서에 적용하지 않는다", async () => {
  const first = { ...snapshot, local_bytes: bytes("first\n"), local_hash:"sha256:b640e840b19d378660b32fb51ae18d67dccb4a8596a29e7bd72c1b2ae5928f41" };
  const second = { ...snapshot, generation: "3", local_bytes: bytes("second\n"), local_hash:"sha256:480c2336b410f1ad5f8bf1b28944490255804b65350c527787e74ebdd511e3a4" };
  const { container, root, save, props } = mount(first);
  const ports: ConflictReviewPorts = { load: vi.fn().mockResolvedValueOnce(first).mockResolvedValueOnce(second) };
  await act(async () => { root.render(createElement(ConflictReview, { ...props, ports })); });
  await vi.waitFor(() => expect(container.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe("first\n"));
  await click(container, '[data-testid="conflict-merge-review"]');
  const oldConfirm = container.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-confirm"]')!;
  await act(async () => { root.render(createElement(ConflictReview, { ...props, ports, generation: "3" })); });
  await vi.waitFor(() => expect(container.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe("second\n"));
  await act(async () => { oldConfirm.click(); });
  expect(save).not.toHaveBeenCalled();
  expect(container.querySelector('[data-testid="conflict-merge-confirm"]')).toBeNull();
  await act(async () => { root.unmount(); });
});

test("합본 확인 응답을 기다리는 동안 연속 클릭해도 선택은 한 번만 요청한다", async () => {
  const selected = { ...snapshot, local_bytes: bytes("first\n"), local_hash:"sha256:b640e840b19d378660b32fb51ae18d67dccb4a8596a29e7bd72c1b2ae5928f41" };
  const { container, root, save, props } = mount(selected);
  let finish!: () => void;
  save.mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
  await act(async () => { root.render(createElement(ConflictReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="이 기기에 저장된 내 초안"]')).not.toBeNull());
  await click(container, '[data-testid="conflict-merge-review"]');
  const confirm = container.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-confirm"]')!;
  await act(async () => { confirm.click(); confirm.click(); });
  expect(save).toHaveBeenCalledExactlyOnceWith(selected);
  expect(confirm.disabled).toBe(true);
  await act(async () => { finish(); });
  await act(async () => { root.unmount(); });
});
