// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { StartupGate } from "./migration-recovery";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));

beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { mocks.invoke.mockReset(); document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("복구 화면이 문서 원문과 복원 요청을 구분하고 종류가 손상돼도 내보내기를 유지한다", async () => {
  const item = (operation_id: string, kind: string | null) => ({ operation_id, document_id: "synthetic-node", binding: null, sequence: "1", generation: kind === "content" ? "1" : null, kind, expected_node_etag: null, expected_meta_etag: null, status: "queued", terminal_kind: null, registration_status: "frozen", metadata_error: kind === null });
  mocks.invoke.mockImplementation(async (command: string) => {
    if (command === "client_startup_status") return { mode: "recovery", renderer_session_id: "owner" };
    if (command === "migration_inventory") return { drafts: [], operations: [item("content", "content"), item("restore", "restore"), item("unknown", null)], next_draft_after_id: null, next_operation_after_id: null, draft_error: null, operation_error: null };
    throw new Error("unexpected command");
  });
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  await act(async () => { root.render(createElement(StartupGate, { ready: () => null })); });
  await vi.waitFor(() => expect(container.querySelectorAll('[data-testid="migration-export-outbox"]')).toHaveLength(3));
  const sections = [...container.querySelectorAll("section")];
  expect(sections.find((section) => section.textContent?.includes("작업 content"))?.textContent).toContain("전송 원문 내보내기");
  expect(sections.find((section) => section.textContent?.includes("작업 restore"))?.textContent).toContain("복원 요청 내보내기");
  expect(sections.find((section) => section.textContent?.includes("작업 unknown"))?.textContent).toContain("종류 확인 불가");
  await act(async () => { root.unmount(); });
});

test("복구 화면이 삭제 요청의 확인 ETag를 표시하고 빈 HTTP 본문을 원문으로 제공하지 않는다", async () => {
  mocks.invoke.mockImplementation(async (command: string) => {
    if (command === "client_startup_status") return { mode: "recovery", renderer_session_id: "owner" };
    if (command === "migration_inventory") return { drafts: [], operations: [{ operation_id: "delete-op", document_id: "synthetic-node", binding: null, sequence: "1", generation: null, kind: "delete", expected_node_etag: '"node-1"', expected_meta_etag: null, status: "queued", terminal_kind: null, registration_status: "frozen", metadata_error: false }], next_draft_after_id: null, next_operation_after_id: null, draft_error: null, operation_error: null };
    throw new Error("unexpected command");
  });
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  await act(async () => { root.render(createElement(StartupGate, { ready: () => null })); });
  await vi.waitFor(() => expect(container.textContent).toContain("삭제 요청"));
  const section = [...container.querySelectorAll("section")].find((item) => item.textContent?.includes("작업 delete-op"));
  expect(section?.textContent).toContain('"node-1"');
  expect(section?.querySelector('[data-testid="migration-view-outbox"]')).toBeNull();
  expect(section?.querySelector('[data-testid="migration-export-outbox"]')).toBeNull();
  await act(async () => { root.unmount(); });
});

test("복구 목록에서 생성 대상의 종류가 없으면 폴더로 단정하지 않고 작업 데이터를 보존한다", async () => {
  mocks.invoke.mockImplementation(async (command: string) => {
    if (command === "client_startup_status") return { mode: "recovery", renderer_session_id: "owner" };
    if (command === "migration_inventory") return { drafts: [], operations: [{ operation_id:"create-op", document_id:"synthetic-node", binding:null, sequence:"1", generation:null, kind:"create", expected_node_etag:null, expected_meta_etag:null, status:"sent", terminal_kind:null, registration_status:"frozen", metadata_error:false }], next_draft_after_id:null, next_operation_after_id:null, draft_error:null, operation_error:null };
    throw new Error("unexpected command");
  });
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(StartupGate, { ready: () => null })); });
  await vi.waitFor(() => expect(container.querySelector('[data-testid="migration-export-outbox"]')).not.toBeNull());
  const section = [...container.querySelectorAll("section")].find((item) => item.textContent?.includes("작업 create-op"));
  expect(section?.textContent).toContain("생성 요청"); expect(section?.textContent).not.toContain("폴더 생성 요청");
  expect(section?.querySelector('[data-testid="migration-view-outbox"]')).not.toBeNull();
  expect(section?.querySelector('[data-testid="migration-export-outbox"]')).not.toBeNull();
  await act(async () => { root.unmount(); });
});
