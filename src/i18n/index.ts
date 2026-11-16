/**
 * Grename 轻量 i18n（无第三方依赖）。
 *
 * 约定：
 *  - 默认语言简体中文（zh-CN），状态名等一切文案默认显示中文；
 *  - 切换语言即时生效，偏好写入 localStorage，并设置 <html lang>；
 *  - 字典缺省键自动回退到中文，新增语言可以只翻译部分文案。
 *
 * 扩展新语言（无需改动任何组件）：
 *  1) 复制 locales/en-US.ts 为 locales/<code>.ts，改成目标语言文案；
 *  2) 在下方 registerLocale({ code, label, dict }) 注册一行。
 *  设置页「语言」下拉会自动列出所有已注册语言。
 */
import { ref } from "vue";
import zhCN from "./locales/zh-CN";
import enUS from "./locales/en-US";

export type Dict = Record<string, string>;
export interface LocaleDef {
  code: string;   // BCP 47，例如 "ja-JP"
  label: string;  // 显示在设置页的原生语言名
  dict: Dict;
}

const registry = new Map<string, LocaleDef>();
export const DEFAULT_LOCALE = "zh-CN";
const FALLBACK: Dict = zhCN;
const STORAGE_KEY = "grename.locale";

export function registerLocale(def: LocaleDef) {
  registry.set(def.code, def);
}

registerLocale({ code: "zh-CN", label: "简体中文", dict: zhCN });
registerLocale({ code: "en-US", label: "English", dict: enUS });
// 扩展点：registerLocale({ code: "ja-JP", label: "日本語", dict: jaJP });

function readStored(): string {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v && registry.has(v)) return v;
  } catch { /* 存储不可用时用默认语言 */ }
  return DEFAULT_LOCALE;
}

/** 响应式当前语言；t() 读取它，因此模板会在切换时自动重渲染。 */
export const locale = ref<string>(readStored());

/** 设置页语言下拉的数据源：由注册表生成，新增语言自动出现。 */
export function getLocaleOptions(): { code: string; label: string }[] {
  return [...registry.values()].map(({ code, label }) => ({ code, label }));
}

/**
 * 取文案。params 用于替换 {name} 占位符；fallback 用于动态代码（状态/阶段名）。
 * 查找顺序：当前语言 → 中文（兜底）→ fallback → key 本身。
 */
export function t(key: string, params?: Record<string, string | number>, fallback?: string): string {
  const d = registry.get(locale.value)?.dict ?? FALLBACK;
  let s = d[key] ?? FALLBACK[key] ?? fallback ?? key;
  if (params) {
    for (const k of Object.keys(params)) s = s.split(`{${k}}`).join(String(params[k]));
  }
  return s;
}

/** 状态值 → 当前语言名称；未收录的后端代码原样显示，不丢信息。 */
export function statusLabel(code: string): string {
  return t(`status.${code}`, undefined, code || "—");
}
/** 进度阶段值 → 当前语言名称。 */
export function phaseLabel(code: string): string {
  return t(`phase.${code}`, undefined, code || "—");
}

export function setLocale(code: string) {
  if (!registry.has(code)) return;
  locale.value = code;
  document.documentElement.lang = code;
  try { localStorage.setItem(STORAGE_KEY, code); } catch { /* 仅本次生效 */ }
}

/** 启动入口：main.ts 在挂载前调用，避免语言相关的首帧闪烁。 */
export function initI18n() {
  document.documentElement.lang = locale.value;
}
