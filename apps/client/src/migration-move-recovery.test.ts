// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { StartupGate } from "./migration-recovery";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));

beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { mocks.invoke.mockReset(); document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("동결 이동 요청의 조건과 작업 데이터를 복구 화면에서 확인한다", async () => {
  mocks.invoke.mockImplementation(async (command: string) => {
    if (command === "client_startup_status") return { mode: "recovery", renderer_session_id: "owner" };
    if (command === "migration_inventory") return { drafts: [], operations: [{ operation_id: "move-op", document_id: "synthetic-node", binding: null, sequence: "1", generation: null, kind: "move", expected_node_etag: null, expected_meta_etag: '"meta-1"', status: "sent", terminal_kind: null, registration_status: "frozen", metadata_error: false }], next_draft_after_id: null, next_operation_after_id: null, draft_error: null, operation_error: null };
    throw new Error("unexpected command");
  });
  const container = document.createElement("div");
  document.body.append(container);
  const root = createRoot(container);
  await act(async () => { root.render(createElement(StartupGate, { ready: () => null })); });
  await vi.waitFor(() => expect(container.textContent).toContain("이동 요청"));
  const section = [...container.querySelectorAll("section")].find((item) => item.textContent?.includes("작업 move-op"));
  expect(section?.textContent).toContain('"meta-1"');
  expect(container.textContent).toContain("조건과 등록 정보를 함께 보존하려면 전체 저장소 사본을 내보내세요");
  expect(section?.querySelector('[data-testid="migration-view-outbox"]')).not.toBeNull();
  expect(section?.querySelector('[data-testid="migration-export-outbox"]')).not.toBeNull();
  await act(async () => { root.unmount(); });
});
