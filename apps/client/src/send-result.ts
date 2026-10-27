export function staleTransmissionResult(requestVersion: number, currentVersion: number, resultBinding: string, activeBindings: readonly string[]): { status: "pending" } | undefined {
  if (requestVersion !== currentVersion || (activeBindings.length > 0 && !activeBindings.includes(resultBinding))) return { status: "pending" };
  return undefined;
}

type ReceiptWaitStatus = "ack_pending" | "auth_required" | "gateway_auth_required";
type ResultIdentity = { operation_id: string | null; document_id: string | null; generation: string | null };
type TrackedIdentity = { scope: { binding: string }; documentId: string; generation: string };

export function receiptWaitResult(status: ReceiptWaitStatus, result: ResultIdentity, binding: string, operation: TrackedIdentity | undefined): { status: ReceiptWaitStatus; operationId?: string } {
  if (result.operation_id && operation && operation.scope.binding === binding && result.document_id === operation.documentId && result.generation === operation.generation) return { status, operationId: result.operation_id };
  return { status };
}
