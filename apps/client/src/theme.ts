import { EditorView } from "@codemirror/view";
import type { ThemeBase, ThemePackage, ThemeStatus, ThemeTokens } from "../../../packages/protocol-types";

export type ResolvedTheme = { base: ThemeBase; tokens: Required<ThemeTokens> };
export const defaultThemeStatus: ThemeStatus = { selection: "light", custom: null, safe_mode: false, warning: null };
const palettes: Record<ThemeBase, Required<ThemeTokens>> = {
  light: {
    "surface.background": "#f6f7f9", "surface.raised": "#ffffff", "text.primary": "#202b36", "text.secondary": "#526477",
    "border.default": "#dce1e7", "accent.background": "#e3eaf5", "accent.text": "#234c80",
    "status.conflict": "#855400", "status.error": "#892d24", "status.success": "#25663b",
    "editor.fontFamily": "system-monospace", "editor.fontSize": 14, "spacing.scale": 1,
  },
  dark: {
    "surface.background": "#16191f", "surface.raised": "#20252e", "text.primary": "#e7eaf0", "text.secondary": "#aab6c4",
    "border.default": "#424b59", "accent.background": "#293e59", "accent.text": "#b9d7ff",
    "status.conflict": "#f1be63", "status.error": "#ffaaa1", "status.success": "#87d6a4",
    "editor.fontFamily": "system-monospace", "editor.fontSize": 14, "spacing.scale": 1,
  },
};
const fonts = { "system-monospace": "ui-monospace, monospace", "system-ui": "system-ui, sans-serif" };
const colorKeys = ["surface.background", "surface.raised", "text.primary", "text.secondary", "border.default", "accent.background", "accent.text", "status.conflict", "status.error", "status.success"] as const;
const requiredColors = ["surface.background", "text.primary", "border.default", "status.conflict"] as const;

function invalid(): never { throw new Error("테마 설정의 형식이나 값이 올바르지 않습니다."); }
function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) invalid();
  return value as Record<string, unknown>;
}
function knownKeys(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).some((key) => !keys.includes(key))) invalid();
}

function themePackage(value: unknown): ThemePackage {
  const item = record(value);
  knownKeys(item, ["id", "schemaVersion", "name", "base", "tokens"]);
  if (typeof item.id !== "string" || !/^[a-zA-Z0-9._-]{1,64}$/u.test(item.id)
    || item.schemaVersion !== 1 || typeof item.name !== "string" || !item.name.trim()
    || [...item.name].length > 80 || [...item.name].some((character) => { const code = character.codePointAt(0)!; return code < 32 || (code >= 127 && code <= 159); })
    || (item.base !== "light" && item.base !== "dark")) invalid();
  const tokens = record(item.tokens);
  knownKeys(tokens, [...colorKeys, "editor.fontFamily", "editor.fontSize", "spacing.scale"]);
  for (const key of colorKeys) {
    if (tokens[key] !== undefined && (typeof tokens[key] !== "string" || !/^#[0-9a-f]{6}$/iu.test(tokens[key]))) invalid();
  }
  if (requiredColors.some((key) => tokens[key] === undefined)) invalid();
  if (tokens["editor.fontFamily"] !== undefined && !Object.hasOwn(fonts, String(tokens["editor.fontFamily"]))) invalid();
  const size = tokens["editor.fontSize"], spacing = tokens["spacing.scale"];
  if (size !== undefined && (typeof size !== "number" || !Number.isInteger(size) || size < 12 || size > 24)) invalid();
  if (spacing !== undefined && (typeof spacing !== "number" || !Number.isFinite(spacing) || spacing < 0.8 || spacing > 1.5)) invalid();
  return item as unknown as ThemePackage;
}

export function themeStatus(value: unknown): ThemeStatus {
  const item = record(value);
  knownKeys(item, ["selection", "custom", "safe_mode", "warning"]);
  if (!["light", "dark", "custom"].includes(String(item.selection)) || typeof item.safe_mode !== "boolean"
    || (item.warning !== null && typeof item.warning !== "string")) invalid();
  const custom = item.custom === null ? null : themePackage(item.custom);
  if (item.selection === "custom" && custom === null) invalid();
  return { selection: item.selection as ThemeStatus["selection"], custom, safe_mode: item.safe_mode, warning: item.warning as string | null };
}

export function resolveTheme(status: ThemeStatus): ResolvedTheme {
  const custom = !status.safe_mode && status.selection === "custom" ? status.custom : null;
  const base = status.safe_mode ? "light" : custom?.base ?? (status.selection === "dark" ? "dark" : "light");
  return { base, tokens: { ...palettes[base], ...custom?.tokens } };
}

export function applyDocumentTheme(theme: ResolvedTheme): void {
  const root = document.documentElement;
  root.dataset.theme = theme.base;
  root.style.colorScheme = theme.base;
  for (const key of colorKeys) root.style.setProperty(`--${key.replaceAll(".", "-")}`, theme.tokens[key]);
  root.style.setProperty("--editor-font-family", fonts[theme.tokens["editor.fontFamily"]]);
  root.style.setProperty("--editor-font-size", `${theme.tokens["editor.fontSize"]}px`);
  root.style.setProperty("--spacing-scale", String(theme.tokens["spacing.scale"]));
}

export function editorTheme(theme: ResolvedTheme) {
  const colors = theme.tokens;
  return EditorView.theme({
    "&": { color: colors["text.primary"], backgroundColor: colors["surface.raised"], fontSize: `${colors["editor.fontSize"]}px` },
    ".cm-scroller": { fontFamily: fonts[colors["editor.fontFamily"]] },
    ".cm-content": { caretColor: colors["text.primary"] },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: colors["text.primary"] },
    ".cm-gutters": { backgroundColor: colors["surface.background"], color: colors["text.secondary"], border: "none" },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": { backgroundColor: colors["accent.background"] },
  }, { dark: theme.base === "dark" });
}
