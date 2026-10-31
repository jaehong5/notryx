import { beforeEach, expect, test, vi } from "vitest";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

beforeEach(() => { invoke.mockReset(); });

test.each([
  ["expiring", "14일"],
  ["expired", "만료 시각"],
  ["auth_required", "다시 등록"],
  ["unsupported", "서버"],
] as const)("인증 연장 상태 %s 안내를 표시한다", async (status, expected) => {
  const { credentialRenewalMessage } = await import("./credential-status");
  expect(credentialRenewalMessage(status)).toContain(expected);
});

test.each([
  ["none", ""],
  ["secure", ""],
  ["session_only", "인증정보를 이 실행 동안만 사용합니다. 앱을 다시 시작하면 다시 등록해야 합니다."],
  ["registration_required", "이전에 저장한 인증정보가 없어 다시 등록해야 합니다. 기기 자료는 보존됩니다."],
  ["unavailable", "저장된 인증정보를 복원하지 못했습니다. 비밀 저장소와 서버 상태를 확인해 주세요."],
  ["identity_unsupported", "서버에서 기기 신원 확인 API를 사용할 수 없어 인증정보를 자동 복원할 수 없습니다. 서버 설정과 버전을 확인해 주세요."],
  ["pending", "비밀 저장소에 대기 중인 등록이 있습니다. 저장소를 확인하고 등록을 복구해 주세요."],
] as const)("인증정보 상태 %s에 맞는 안내를 표시한다", async (status, message) => {
  const { credentialStatusMessage } = await import("./credential-status");
  expect(credentialStatusMessage(status)).toBe(message);
});

test.each(["none", "secure", "session_only", "registration_required", "unavailable", "identity_unsupported", "pending"] as const)("현재 renderer로 상태 %s를 확인한다", async (status) => {
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve({ status, renewal_status: "none" }));
  const { workspaceCredentialStatus } = await import("./credential-status");
  await expect(workspaceCredentialStatus()).resolves.toEqual({ status, renewal_status: "none" });
  expect(invoke).toHaveBeenCalledWith("workspace_credential_status", { request: { renderer_session_id: "renderer" } });
});

test.each([null, [], {}, { status: "unknown" }, { status: null }, { status: "secure" }, { status: "secure", renewal_status: "unknown" }])("잘못된 인증정보 상태 응답을 거절한다", async (response) => {
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(response));
  const { workspaceCredentialStatus } = await import("./credential-status");
  await expect(workspaceCredentialStatus()).rejects.toThrow("인증정보 저장 상태를 확인하지 못했습니다.");
});
