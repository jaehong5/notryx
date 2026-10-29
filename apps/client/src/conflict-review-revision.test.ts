// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { ConflictReview, type ConflictReviewPorts } from "./conflict-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const bytes = (text:string) => new TextEncoder().encode(text);
beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("충돌 이후 서버 본문이 달라질 수 있음을 설명하고 세 원문을 구분한다", async () => {
  const ports: ConflictReviewPorts = { load:vi.fn(async()=>({ binding, node_id:"document", operation_id:"operation", operation_seq:"1", generation:"2", base_revision:"base", base_bytes:bytes("base"), base_hash:"sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2", local_bytes:bytes("local"), local_hash:"sha256:25bf8e1a2393f1108d37029b3df5593236c755742ec93465bbafa9b290bddcf6", conflict_revision:"revision-at-412", remote_revision:"current-rev", remote_bytes:bytes("current"), remote_hash:"sha256:97b0560280ed60a5a1eaa1bc45492543c8a986ad5a25b468c427eb83c3e88191" })) };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(ConflictReview, { binding, documentId:"document", sessionId:"session", generation:"2", operationId:"operation", ports, onClose:vi.fn() })); });
  await vi.waitFor(() => expect(container.querySelector('[aria-label="조회 당시 서버 본문"]')).not.toBeNull());
  expect(container.textContent).toContain("서버 본문은 충돌이 발생한 뒤 달라졌을 수 있습니다.");
  expect(container.textContent).not.toContain("revision-at-412");
  expect(container.textContent).not.toContain("current-rev");
  for (const [label, value] of [["기준 원문", "base"], ["이 기기에 저장된 내 초안", "local"], ["조회 당시 서버 본문", "current"]]) {
    const field = container.querySelector<HTMLTextAreaElement>(`[aria-label="${label}"]`);
    expect(field?.value).toBe(value);
    expect(field?.readOnly).toBe(true);
  }
  await act(async () => { root.unmount(); });
});
