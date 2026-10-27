import { EditorState, Prec, StateField, type Extension, type Text, type Transaction } from "@codemirror/state";
import { MAX_EDIT_BYTES } from "./source";

const encoder = new TextEncoder();

function byteLength(text: string): number {
  return encoder.encode(text).length;
}

function isSurrogate(text: Text, position: number): boolean {
  if (position < 0 || position >= text.length) return false;
  const code = text.sliceString(position, position + 1).charCodeAt(0);
  return code >= 0xd800 && code <= 0xdfff;
}

function changedByteLength(transaction: Transaction, current: number, bomBytes: number): number {
  let delta = 0;
  let needsFullCount = false;
  const previous = transaction.startState.doc;
  transaction.changes.iterChanges((from, to, fromB, toB, inserted) => {
    if (isSurrogate(previous, from - 1) || isSurrogate(previous, from) ||
        isSurrogate(previous, to - 1) || isSurrogate(previous, to) ||
        isSurrogate(inserted, 0) || isSurrogate(inserted, toB - fromB - 1)) {
      needsFullCount = true;
      return;
    }
    delta += byteLength(inserted.toString()) - byteLength(previous.sliceString(from, to));
  });
  return needsFullCount ? byteLength(transaction.newDoc.toString()) + bomBytes : current + delta;
}

export function documentSizeGuard(bom: boolean, onRejected: () => void): Extension {
  const bomBytes = bom ? 3 : 0;
  const documentBytes = StateField.define<number>({
    create: (state) => byteLength(state.doc.toString()) + bomBytes,
    update: (current, transaction) => transaction.docChanged ? changedByteLength(transaction, current, bomBytes) : current,
  });
  const filter = EditorState.transactionFilter.of((transaction) => {
    if (!transaction.docChanged) return transaction;
    if (transaction.newDoc.length + bomBytes <= MAX_EDIT_BYTES &&
        changedByteLength(transaction, transaction.startState.field(documentBytes), bomBytes) <= MAX_EDIT_BYTES) return transaction;
    onRejected();
    return [];
  });
  return [documentBytes, Prec.highest(filter)];
}
