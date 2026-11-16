import { createApp } from "vue";
import App from "./App.vue";
import { initTheme } from "./theme";
import { initI18n } from "./i18n/index";
import { currentMonitor, getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

/** 与 tauri.conf.json 的默认窗口尺寸 / 最小尺寸保持一致（逻辑像素）。 */
const WIN_PREF_W = 1120;
const WIN_PREF_H = 700;
const WIN_MIN_W = 860;
const WIN_MIN_H = 520;
/** 四周留白，避免无边框窗口贴边 / 被任务栏或屏幕边缘裁切。 */
const WIN_MARGIN = 24;

/**
 * 启动时按「当前显示器工作区」再夹一次窗口尺寸：
 * 1366×768 一类的笔记本屏、或 125%/150% 缩放下，静态默认尺寸仍可能超出屏幕。
 * 只做收缩与居中，不读写任何业务状态；非 Tauri 环境（vite dev 浏览器）静默跳过。
 * 所需权限见 capabilities/default.json 的 core:window:allow-* 三项。
 */
async function fitWindowToScreen() {
  try {
    const monitor = await currentMonitor();
    if (!monitor) return;
    const sf = monitor.scaleFactor || 1;
    const availW = Math.floor(monitor.workArea.size.width / sf);
    const availH = Math.floor(monitor.workArea.size.height / sf);
    const w = Math.min(WIN_PREF_W, Math.max(WIN_MIN_W, availW - WIN_MARGIN));
    const h = Math.min(WIN_PREF_H, Math.max(WIN_MIN_H, availH - WIN_MARGIN));
    const win = getCurrentWindow();
    await win.setSize(new LogicalSize(w, h));
    await win.center();
  } catch {
    /* 非 Tauri 环境或权限缺失：保持 tauri.conf.json 的默认尺寸，不影响界面初始化 */
  }
}

// 先定主题与语言再挂载，避免首帧闪白 / 文案跳变。
initTheme();
initI18n();
createApp(App).mount("#app");
// 窗口在配置里初始隐藏：等 Vue 挂载完（启动页已进 DOM 且不透明）再显示，
// 保证用户看到的第一帧一定是 loading 页，而不是主界面。
void (async () => {
  await fitWindowToScreen();
  try { await getCurrentWindow().show(); } catch { /* 浏览器 dev 环境无窗口，忽略 */ }
})();
