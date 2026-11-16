/**
 * 主题风格：跟随系统 / 暗色 / 明亮。
 *
 * 全站暗色样式统一由 <html data-theme="dark|light"> 驱动（不再是 prefers-color-scheme
 * 媒体查询），这样「跟随系统」与手动指定可以共用同一套配色规则。
 * main.ts 在创建 Vue 应用前调用 initTheme()，保证首帧不闪白。
 */
export type ThemeMode = "system" | "dark" | "light";

const STORAGE_KEY = "grename.theme";
let mq: MediaQueryList | null = null;
let onChange: ((e: MediaQueryListEvent) => void) | null = null;

export function getThemeMode(): ThemeMode {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === "system" || v === "dark" || v === "light") return v;
  } catch { /* 存储不可用时用默认值 */ }
  return "system";
}

function resolve(mode: ThemeMode): "dark" | "light" {
  if (mode !== "system") return mode;
  const dark = typeof window.matchMedia === "function"
    && window.matchMedia("(prefers-color-scheme: dark)").matches;
  return dark ? "dark" : "light";
}

/** 应用主题并（可选）持久化；system 模式下监听系统配色变化。 */
export function applyTheme(mode: ThemeMode, persist = false) {
  document.documentElement.dataset.theme = resolve(mode);

  if (mq && onChange) {
    mq.removeEventListener("change", onChange);
    mq = null;
    onChange = null;
  }
  if (mode === "system" && typeof window.matchMedia === "function") {
    mq = window.matchMedia("(prefers-color-scheme: dark)");
    onChange = () => {
      document.documentElement.dataset.theme = mq?.matches ? "dark" : "light";
    };
    mq.addEventListener("change", onChange);
  }

  if (persist) {
    try { localStorage.setItem(STORAGE_KEY, mode); } catch { /* 存储不可用时仅本次生效 */ }
  }
}

/** 启动入口：读取已保存的偏好并应用。 */
export function initTheme() {
  applyTheme(getThemeMode());
}

/** 设置页选择后调用：立即生效 + 持久化。 */
export function setTheme(mode: ThemeMode) {
  applyTheme(mode, true);
}
