// @vitest-environment jsdom
import { act, createElement, useState } from "react";
import { createRoot } from "react-dom/client";
import { expect, test, vi } from "vitest";
import { AutoTransmit, type TransmitScope } from "./auto-transmit";
import { clearQueueError, putQueueError, QueueAlerts, type QueueError } from "./queue-errors";

test("failed drafts remain visible across document changes and only a recovered draft is cleared", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const a: TransmitScope = { binding: "workspace-1", documentId: "document-a", editSessionId: "session-a" };
  const b: TransmitScope = { binding: "workspace-1", documentId: "document-b", editSessionId: "session-b" };
  const reopenedA = { ...a, editSessionId: "session-a-reopened" };
  const foreignA = { ...a, binding: "workspace-2", editSessionId: "foreign-a" };
  let report!: (scope: TransmitScope, message: string) => void;
  let recovered!: (scope: TransmitScope) => void;
  function Harness({ selected }: { selected: string }) {
    const [errors, setErrors] = useState<QueueError[]>([]);
    report = (scope, message) => setErrors((current) => putQueueError(current, scope, message));
    recovered = (scope) => setErrors((current) => clearQueueError(current, scope));
    return createElement("div", { "data-selected": selected }, createElement(QueueAlerts, { error: "기기 인증이 필요합니다.", queueErrors: errors }));
  }
  const container = document.createElement("div");
  const root = createRoot(container);
  await act(async () => root.render(createElement(Harness, { selected: "document-a" })));
  const scheduler = new AutoTransmit({
    queue: vi.fn(async (scope, generation) => {
      if (scope.editSessionId !== reopenedA.editSessionId) throw "Storage";
      return { operationId: "op-a", documentId: scope.documentId, generation };
    }),
    transmit: async () => ({ status: "idle" }),
    acknowledged: () => {},
    authRequired: () => {},
    failed: () => {},
    queueStorageFailed: (scope) => report(scope, "전송 준비 실패"),
    queueRecovered: recovered,
  });
  scheduler.activateBinding(a.binding);
  scheduler.openSession(a);
  await act(async () => { await expect(scheduler.restore(a, "1")).rejects.toThrow("Storage"); });
  await scheduler.deactivatePreservingDraft(a, async () => {});
  await act(async () => root.render(createElement(Harness, { selected: "document-b" })));
  scheduler.openSession(b);
  await act(async () => { await expect(scheduler.restore(b, "1")).rejects.toThrow("Storage"); });
  await scheduler.deactivatePreservingDraft(b, async () => {});
  await act(async () => report(foreignA, "다른 작업 공간의 실패"));
  expect([...container.querySelectorAll('[role="alert"]')].map((alert) => alert.textContent)).toEqual([
    "기기 인증이 필요합니다.", "document-a: 전송 준비 실패", "document-b: 전송 준비 실패", "document-a: 다른 작업 공간의 실패",
  ]);
  scheduler.openSession(reopenedA);
  await act(async () => { await scheduler.restore(reopenedA, "1"); });
  expect([...container.querySelectorAll('[role="alert"]')].map((alert) => alert.textContent)).toEqual([
    "기기 인증이 필요합니다.", "document-b: 전송 준비 실패", "document-a: 다른 작업 공간의 실패",
  ]);
  await scheduler.deactivate(reopenedA);
  await scheduler.dispose();
  await act(async () => root.unmount());
  vi.unstubAllGlobals();
});
