import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { ThemeSelection } from "../../../packages/protocol-types";
import { defaultThemeStatus, resolveTheme, themeStatus } from "./theme";

function failureMessage(error: unknown): string {
  const code = String(error);
  if (code.includes("too_large")) return "테마 파일은 256 KiB 이하여야 합니다.";
  if (code.includes("safe_mode")) return "안전 모드에서는 기본 테마를 사용합니다.";
  if (/write|persist|save|storage|io_error/u.test(code)) return "테마 설정을 저장하지 못했습니다. 이전 테마를 유지합니다.";
  return "테마 파일의 형식이나 값을 확인해 주세요. 이전 테마를 유지합니다.";
}

export function useTheme() {
  const [status, setStatus] = useState(defaultThemeStatus);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const active = useRef(false);
  const inFlight = useRef(false);
  useEffect(() => {
    active.current = true;
    void Promise.resolve().then(() => invoke("theme_status")).then(themeStatus).then((value) => {
      if (active.current) setStatus(value);
    }).catch(() => { if (active.current) setError("테마 설정을 읽지 못해 기본 테마를 사용합니다."); })
      .finally(() => { if (active.current) setReady(true); });
    return () => { active.current = false; };
  }, []);
  async function change(operation: () => Promise<unknown>): Promise<void> {
    if (!ready || inFlight.current) return;
    inFlight.current = true;
    setBusy(true); setError("");
    try {
      const next = themeStatus(await operation());
      if (active.current) setStatus(next);
    } catch (cause) { if (active.current) setError(failureMessage(cause)); }
    finally { inFlight.current = false; if (active.current) setBusy(false); }
  }
  async function importFile(file: File): Promise<void> {
    if (!ready || inFlight.current) return;
    if (file.size > 256 * 1024) { setError("테마 파일은 256 KiB 이하여야 합니다."); return; }
    await change(async () => {
      const json = new TextDecoder("utf-8", { fatal: true }).decode(await file.arrayBuffer());
      if (!active.current) throw new Error("theme_unavailable");
      return invoke("theme_import", { json });
    });
  }
  const theme = useMemo(() => resolveTheme(status), [status]);
  return { status, theme, ready, busy, error, importFile,
    select: (selection: ThemeSelection) => change(() => invoke("theme_select", { selection })), reset: () => change(() => invoke("theme_reset", undefined)) };
}

export function ThemeControls({ controller }: { controller: ReturnType<typeof useTheme> }) {
  const { status, ready, busy, error } = controller;
  const disabled = !ready || busy;
  return <details className="theme-controls">
    <summary>테마</summary>
    <div className="theme-settings">
      <label>화면 테마<select id="theme-select" value={status.safe_mode ? "light" : status.selection} disabled={disabled || status.safe_mode}
        onChange={(event) => void controller.select(event.target.value as ThemeSelection)}>
        <option value="light">밝은 테마</option><option value="dark">어두운 테마</option>
        {status.custom && <option value="custom">사용자: {status.custom.name}</option>}
      </select></label>
      <label>JSON 테마 가져오기<input id="theme-file" type="file" accept=".json,application/json" disabled={disabled || status.safe_mode}
        onChange={(event) => { const file = event.target.files?.[0]; event.target.value = ""; if (file) void controller.importFile(file); }} /></label>
      <button id="theme-reset" disabled={disabled} onClick={() => void controller.reset()}>기본 테마로 복구</button>
      {(status.safe_mode || status.warning) && <p role="status">{status.safe_mode ? "안전 모드: 저장한 테마를 적용하지 않습니다. 기본 테마로 복구한 뒤 앱을 일반 모드로 다시 실행할 수 있습니다." : "저장한 테마 설정을 읽을 수 없어 기본 테마를 사용합니다. 기존 설정 파일은 보존했습니다."}</p>}
      {error && <p role="alert">{error}</p>}
    </div>
  </details>;
}
