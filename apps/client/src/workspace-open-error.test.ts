import { expect, test } from "vitest";
import { workspaceOpenError } from "./workspace-open-error";

test.each([
  ["Network", "서버에 연결할 수 없습니다"],
  ["node_deleted", "삭제된 문서입니다"],
  ["stale_content", "문서가 계속 변경되고 있습니다"],
  ["Changed", "문서가 계속 변경되고 있습니다"],
  ["stale_sync", "동기화 상태를 확인"],
  ["resync_required", "동기화 상태를 확인"],
  ["unexpected_internal_error", "문서를 열지 못했습니다"],
])("shows a useful open error without internal code for %s", (error, expected) => {
  const message = workspaceOpenError(error);
  expect(message).toContain(expected);
  expect(message).not.toContain(error);
});
