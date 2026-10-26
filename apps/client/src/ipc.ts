import { invoke } from "@tauri-apps/api/core";
import type { M0OpenedDocument, M0SavedCheckpoint, M0SaveRequest, M0CloseRequest } from "../../../packages/protocol-types";

function object(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("잘못된 저장 응답입니다.");
  return value as Record<string, unknown>;
}

function string(value: unknown): string {
  if (typeof value !== "string") throw new Error("잘못된 저장 응답입니다.");
  return value;
}

function generation(value: unknown): string {
  const text = string(value);
  if (!/^(0|[1-9][0-9]*)$/.test(text) || BigInt(text) > 9223372036854775807n) throw new Error("잘못된 편집 세대입니다.");
  return text;
}

export async function openDocument(document_id: string): Promise<M0OpenedDocument> {
  const value = object(await invoke("m0_open", { request: { document_id } }));
  const reply = {
    document_id: string(value.document_id), edit_session_id: string(value.edit_session_id),
    generation: generation(value.generation), bytes_base64: string(value.bytes_base64),
  };
  if (reply.document_id !== document_id) throw new Error("다른 문서의 응답입니다.");
  return reply;
}

export async function saveDocument(request: M0SaveRequest): Promise<M0SavedCheckpoint> {
  const value = object(await invoke("m0_save", { request }));
  return { edit_session_id: string(value.edit_session_id), generation: generation(value.generation) };
}

export const closeDocument = (request: M0CloseRequest) => invoke<void>("m0_close", { request });

export function fromBase64(base64: string): Uint8Array {
  if (base64.length > Math.ceil(5 * 1024 * 1024 / 3) * 4) throw new Error("문서가 너무 큽니다.");
  return Uint8Array.from(atob(base64), (char) => char.charCodeAt(0));
}

export function toBase64(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 8192) binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
  return btoa(binary);
}
