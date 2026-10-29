// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenContentReview, type FrozenContentReviewItem, type FrozenContentReviewPorts, type FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "current-device", device_stream_id: "current-stream" };
const old: WorkspaceRegistration = { ...current, device_id: "old-device", device_stream_id: "old-stream" };
const item: FrozenContentReviewItem = { old_binding: old, operation_id: "old-operation", operation_seq: "1", document_id: "document", generation: "1", expected_revision: "base-rev", status: "sent", abandon_state: "none" };
const bytes = (value: string) => new TextEncoder().encode(value);
type LocalSnapshot = Omit<FrozenContentReviewSnapshot, "remote_bytes" | "remote_revision" | "inspected"> & { dependency: string | null; remote_status: "deleted" | "too_large" | "unknown"; remote_index_error: boolean };
const local: LocalSnapshot = { ...item, binding: current, dependency: null, expected_revision: "base-rev", local_generation: "2", outcome_unknown: true, base_revision: "base-rev", operation_bytes: bytes("old payload\n"), base_bytes: bytes("old base\n"), local_bytes: bytes("latest draft\n"), remote_status: "unknown", remote_index_error: false };
type DismissResult = { binding: WorkspaceRegistration; old_operation_id: string };
type LocalPorts = FrozenContentReviewPorts & {
  loadLocal: (binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, operationId: string) => Promise<LocalSnapshot>;
  dismissLocal: (review: LocalSnapshot) => Promise<DismissResult>;
};
function ports(review = local): LocalPorts {
  return {
    list: vi.fn(async (binding) => ({ binding, entries: [item], next_after_id: null })),
    status: vi.fn(async () => null),
    load: vi.fn(async () => { throw new Error("remote_content_unavailable"); }),
    loadLocal: vi.fn(async () => review),
    apply: vi.fn(async () => ({ binding: current, operation_id: "new-operation" })),
    dismissLocal: vi.fn(async () => ({ binding: current, old_operation_id: item.operation_id })),
  };
}
async function mount(api: LocalPorts) {
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.querySelector('[data-testid="frozen-content-draft"] textarea')).not.toBeNull());
  return { container, root };
}
beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("원격 인덱스 오류에서는 로컬 원문을 표시하되 포기를 막고 재동기화를 안내한다", async () => {
  const review = { ...local, remote_status: "unknown" as const, remote_index_error: true };
  const api = ports(review); const { container, root } = await mount(api);
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-payload"] textarea')?.value).toBe("old payload\n");
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-base"] textarea')?.value).toBe("old base\n");
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-draft"] textarea')?.value).toBe("latest draft\n");
  expect(container.textContent).toMatch(/(인덱스|동기화).{0,25}(손상|오류|실패)/s);
  expect(container.textContent).toMatch(/재동기화|다시 동기화/);
  expect(container.textContent).toMatch(/미확인|확인하지 못|확인되지/);
  expect(container.textContent).not.toMatch(/서버.{0,20}(삭제됐|삭제되었|삭제됨)/s);
  expect(container.querySelector('[data-testid="frozen-content-remote"] textarea')).toBeNull();
  const checkbox = container.querySelector<HTMLInputElement>('[data-testid="frozen-content-select-dismiss"]');
  const button = container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-dismiss"]');
  expect(checkbox === null || checkbox.disabled).toBe(true);
  expect(button === null || button.disabled).toBe(true);
  if (checkbox) await act(async () => { checkbox.click(); });
  if (button) await act(async () => { button.click(); });
  expect(api.dismissLocal).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("원격 인덱스 오류와 삭제 확정 증거가 함께 오면 포기 자료로 사용하지 않는다", async () => {
  const api = ports({ ...local, remote_status: "deleted", remote_index_error: true } as LocalSnapshot & { remote_index_error: boolean });
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.textContent).toMatch(/불러오지 못|확인하지 못|오류|손상/));
  expect(container.textContent).not.toMatch(/서버.{0,20}(삭제됐|삭제되었|삭제됨)/s);
  expect(api.dismissLocal).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  const button = container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-dismiss"]');
  expect(button === null || button.disabled).toBe(true);
  await act(async () => { root.unmount(); });
});

test("인덱스가 정상이고 원격 상태만 미확인이면 포기 선택을 허용한다", async () => {
  const api = ports({ ...local, remote_index_error: false } as LocalSnapshot & { remote_index_error: boolean });
  const { container, root } = await mount(api);
  expect(container.textContent).toMatch(/미확인|확인하지 못|확인되지/);
  const checkbox = container.querySelector<HTMLInputElement>('[data-testid="frozen-content-select-dismiss"]');
  expect(checkbox).not.toBeNull(); expect(checkbox!.disabled).toBe(false);
  await act(async () => { checkbox!.click(); });
  const button = container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-dismiss"]');
  expect(button).not.toBeNull(); expect(button!.disabled).toBe(false);
  await act(async () => { root.unmount(); });
});
