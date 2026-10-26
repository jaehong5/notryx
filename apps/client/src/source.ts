export type Source = {
  original: Uint8Array;
  text: string;
  bom: boolean;
  newline: "\n" | "\r\n";
  readonly: boolean;
  reason?: string;
};

export function openSource(bytes: Uint8Array): Source {
  const original = bytes.slice();
  const bom = bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf;
  let decoded: string;
  let reason: string | undefined;
  try {
    decoded = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    decoded = new TextDecoder().decode(bytes);
    reason = "UTF-8이 아닌 문서는 원본을 보존한 채 읽기 전용으로 엽니다.";
  }
  const hasCrLf = decoded.includes("\r\n");
  const withoutCrLf = decoded.replace(/\r\n/g, "");
  if (withoutCrLf.includes("\r") || (hasCrLf && withoutCrLf.includes("\n"))) {
    reason ??= "혼합 줄바꿈은 읽기 전용입니다. 원본 bytes를 그대로 보존합니다.";
  }
  if (bytes.length > 5 * 1024 * 1024) reason ??= "편집 가능한 최대 크기는 5 MiB입니다.";
  return {
    original,
    text: decoded.replace(/\r\n?/g, "\n"),
    bom,
    newline: hasCrLf ? "\r\n" : "\n",
    readonly: reason !== undefined,
    reason,
  };
}

export function saveSource(source: Source, text: string): Uint8Array {
  if (text === source.text) return source.original.slice();
  if (source.readonly) throw new Error(source.reason);
  if (/[\uD800-\uDFFF]/u.test(text)) throw new Error("완성되지 않은 Unicode 문자는 저장할 수 없습니다.");
  if (text.includes("\r")) throw new Error("편집기 줄바꿈은 LF여야 합니다.");
  const raw = (source.bom ? "\uFEFF" : "") + text.replace(/\n/g, source.newline);
  const bytes = new TextEncoder().encode(raw);
  if (bytes.length > 5 * 1024 * 1024) throw new Error("편집 가능한 최대 크기는 5 MiB입니다.");
  return bytes;
}
