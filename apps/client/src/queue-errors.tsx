import type { TransmitScope } from "./auto-transmit";

export type QueueError = { scope: TransmitScope; message: string };

export function clearQueueError(errors: QueueError[], scope: TransmitScope): QueueError[] {
  return errors.filter((error) => error.scope.binding !== scope.binding || error.scope.documentId !== scope.documentId);
}

export function putQueueError(errors: QueueError[], scope: TransmitScope, message: string): QueueError[] {
  return [...clearQueueError(errors, scope), { scope, message }];
}

export function QueueAlerts({ error, queueErrors }: { error: string; queueErrors: QueueError[] }) {
  return <>
    {error && <p role="alert">{error}</p>}
    {queueErrors.map(({ scope, message }) => <p role="alert" key={JSON.stringify([scope.binding, scope.documentId])}>{scope.documentId}: {message}</p>)}
  </>;
}
