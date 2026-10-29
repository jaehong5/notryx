export function workspaceOpenError(cause: unknown): string {
  const code = String(cause).replace(/^Error:\s*/, "");
  switch (code) {
    case "Network":
    case "Unavailable":
    case "not_connected":
      return "서버에 연결할 수 없습니다. 연결 상태를 확인하고 다시 열어 주세요.";
    case "node_deleted":
      return "삭제된 문서입니다. 목록을 갱신해 주세요.";
    case "Changed":
    case "stale_content":
      return "문서가 계속 변경되고 있습니다. 목록을 갱신한 뒤 다시 열어 주세요.";
    case "stale_sync":
    case "resync_required":
    case "bootstrap_required":
    case "server_not_ready":
      return "동기화 상태를 확인한 뒤 문서를 다시 열어 주세요.";
    default:
      return "문서를 열지 못했습니다. 상태를 확인한 뒤 다시 시도해 주세요.";
  }
}
