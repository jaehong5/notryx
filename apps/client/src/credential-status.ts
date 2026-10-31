import { invoke } from "@tauri-apps/api/core";
import type { WorkspaceCredentialStatus, WorkspaceCredentialState, WorkspaceCredentialRenewalStatus } from "../../../packages/protocol-types";

export function credentialStatusMessage(status: WorkspaceCredentialStatus): string {
  switch (status) {
    case "none":
    case "secure": return "";
    case "session_only": return "인증정보를 이 실행 동안만 사용합니다. 앱을 다시 시작하면 다시 등록해야 합니다.";
    case "registration_required": return "이전에 저장한 인증정보가 없어 다시 등록해야 합니다. 기기 자료는 보존됩니다.";
    case "unavailable": return "저장된 인증정보를 복원하지 못했습니다. 비밀 저장소와 서버 상태를 확인해 주세요.";
    case "identity_unsupported": return "서버에서 기기 신원 확인 API를 사용할 수 없어 인증정보를 자동 복원할 수 없습니다. 서버 설정과 버전을 확인해 주세요.";
    case "pending": return "비밀 저장소에 대기 중인 등록이 있습니다. 저장소를 확인하고 등록을 복구해 주세요.";
  }
}

export function credentialRenewalMessage(status: WorkspaceCredentialRenewalStatus): string {
  switch (status) {
    case "none": return "";
    case "expiring": return "기기 인증 만료까지 14일 이하입니다. 서버 연결을 확인해 주세요. 자동 연장이 안 되면 다시 등록해야 합니다.";
    case "expired": return "기기 인증의 저장된 만료 시각이 지났습니다. 서버에 연결해 확인하고 필요하면 다시 등록해 주세요. 기기 자료는 보존됩니다.";
    case "auth_required": return "기기 인증이 만료되었거나 폐기되었습니다. 다시 등록해 주세요. 기기 자료는 보존됩니다.";
    case "unsupported": return "서버가 인증 자동 연장을 지원하지 않습니다. 서버를 업데이트하거나 만료 전에 기기를 다시 등록해 주세요.";
  }
}

export function credentialStateMessage(state: WorkspaceCredentialState): string {
  const renewal = credentialRenewalMessage(state.renewal_status);
  if (state.renewal_status === "auth_required") return renewal;
  return [credentialStatusMessage(state.status), renewal].filter(Boolean).join(" ");
}

export async function workspaceCredentialStatus(): Promise<WorkspaceCredentialState> {
  const rendererSession: unknown = await invoke("m0_renderer_session");
  if (typeof rendererSession !== "string" || !rendererSession) throw new Error("인증정보 저장 상태를 확인하지 못했습니다.");
  const result: unknown = await invoke("workspace_credential_status", { request: { renderer_session_id: rendererSession } });
  if (result === null || typeof result !== "object" || Array.isArray(result) || !("status" in result)) throw new Error("인증정보 저장 상태를 확인하지 못했습니다.");
  const status = result.status;
  if (status !== "none" && status !== "secure" && status !== "session_only" && status !== "registration_required" && status !== "unavailable" && status !== "identity_unsupported" && status !== "pending") throw new Error("인증정보 저장 상태를 확인하지 못했습니다.");
  if (!("renewal_status" in result)) throw new Error("인증정보 저장 상태를 확인하지 못했습니다.");
  const renewal = result.renewal_status;
  if (renewal !== "none" && renewal !== "expiring" && renewal !== "expired" && renewal !== "auth_required" && renewal !== "unsupported") throw new Error("인증정보 저장 상태를 확인하지 못했습니다.");
  return { status, renewal_status: renewal };
}
