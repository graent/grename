<script setup lang="ts">
/**
 * 自绘标题栏（tauri.conf.json 中 decorations=false）。
 * 按钮顺序：置顶 / 设置 / 最小化 / 关闭（不含最大化）。
 * 设置图标 → 第一层下拉菜单 SettingsMenu；点菜单项 → 第二层 Modal SettingsPopover。
 * 本组件不触碰任何业务逻辑，仅负责窗口外观与窗口控制。
 */
import { onMounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Minus, Pin, PinOff, Settings, X } from "@lucide/vue";
import SettingsMenu from "./SettingsMenu.vue";
import SettingsPopover from "./SettingsPopover.vue";
import type { PageKey } from "./settings";
import { t } from "../i18n/index";
import { APP_VERSION } from "../version";

const showMenu = ref(false);
const dialogPage = ref<PageKey | null>(null);
const pinned = ref(false);

function toggleMenu() {
  if (dialogPage.value) { dialogPage.value = null; showMenu.value = false; return; }
  showMenu.value = !showMenu.value;
}
function openDialog(key: PageKey) {
  dialogPage.value = key;
}
async function togglePin() {
  const next = !pinned.value;
  try {
    await getCurrentWindow().setAlwaysOnTop(next);
    pinned.value = next;
  } catch { /* 窗口已销毁或无权限时保持原状态 */ }
}
async function minimize() {
  showMenu.value = false;
  try { await getCurrentWindow().minimize(); } catch { /* 窗口已销毁 */ }
}
async function close() {
  showMenu.value = false;
  try { await getCurrentWindow().close(); } catch { /* 窗口已销毁 */ }
}
onMounted(async () => {
  try { pinned.value = await getCurrentWindow().isAlwaysOnTop(); } catch { /* 用默认值 false */ }
});
</script>

<template>
  <div class="titlebar">
    <div class="tb-brand" data-tauri-drag-region>
      <img class="tb-logo" src="/grename-icon.svg" alt="" draggable="false" />
      <span class="tb-name">Grename</span>
      <span class="tb-dot">·</span>
      <span class="tb-desc">{{ t("tb.desc") }}</span>
      <span class="tb-ver">v{{ APP_VERSION }}</span>
    </div>

    <div class="tb-drag" data-tauri-drag-region @dblclick.stop.prevent></div>

    <div class="tb-actions">
      <button
        class="tb-btn"
        :class="{ 'tb-pin-on': pinned }"
        :title="pinned ? t('tb.pinOn') : t('tb.pinOff')"
        :aria-label="pinned ? t('tb.pinOn') : t('tb.pinOff')"
        :aria-pressed="pinned"
        @click="togglePin"
      >
        <PinOff v-if="pinned" :size="16" :stroke-width="1.8" />
        <Pin v-else :size="16" :stroke-width="1.8" />
      </button>
      <button
        class="tb-btn"
        :class="{ 'tb-on': showMenu }"
        data-menu-trigger
        :title="t('tb.settings')"
        :aria-label="t('tb.settings')"
        @click="toggleMenu"
      >
        <Settings :size="16" :stroke-width="1.8" />
      </button>
      <button class="tb-btn" :title="t('tb.minimize')" :aria-label="t('tb.minimize')" @click="minimize">
        <Minus :size="17" :stroke-width="1.8" />
      </button>
      <button class="tb-btn tb-close" :title="t('tb.close')" :aria-label="t('tb.close')" @click="close">
        <X :size="17" :stroke-width="1.8" />
      </button>
    </div>

    <SettingsMenu
      v-if="showMenu"
      @open="openDialog"
      @close="showMenu = false"
    />
    <SettingsPopover
      v-if="dialogPage"
      :initial="dialogPage"
      @close="dialogPage = null"
    />
  </div>
</template>

<style scoped>
/* 选择器统一带 .titlebar 前缀，压过 App.vue 的全局 button 规则 */
.titlebar {
  position: relative;
  z-index: 20; /* 菜单/弹层之上不参与滚动：作为 shell 的固定首行 */
  flex: 0 0 40px;
  display: flex;
  align-items: center;
  height: 40px;
  background: #fff;
  border-bottom: 1px solid #dce7e5;
  user-select: none;
  -webkit-user-select: none;
}
.titlebar .tb-brand {
  display: flex;
  align-items: center;
  gap: 8px;
  padding-left: 14px;
  flex: 0 0 auto;
}
/* 让 mousedown 的 target 落在带拖拽属性的父元素上 */
.titlebar .tb-brand > * { pointer-events: none; }
/* 标题前置图标：与桌面 / 任务栏 / 启动页共用 public/grename-icon.svg */
.titlebar .tb-logo {
  display: block;
  width: 20px;
  height: 20px;
  flex: 0 0 20px;
  object-fit: contain;
  border-radius: 5px;
}
.titlebar .tb-name { font-size: 13px; font-weight: 600; color: #0f5f5c; }
.titlebar .tb-dot { color: #a8c0bd; }
.titlebar .tb-desc { font-size: 12px; color: #617473; }
/* 「批量重命名」后的小字版本号，弱于标题描述 */
.titlebar .tb-ver { font-size: 10.5px; color: #9ab5b1; letter-spacing: .2px; margin-left: 1px; }
.titlebar .tb-drag { flex: 1; height: 100%; }
.titlebar .tb-actions { display: flex; align-items: stretch; height: 100%; flex: 0 0 auto; }
.titlebar .tb-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 46px;
  height: 100%;
  padding: 0;
  border: 0;
  border-radius: 0;
  background: transparent;
  color: #4c605e;
}
.titlebar .tb-btn:hover:not(:disabled) { background: #eef5f4; color: #0f5f5c; }
.titlebar .tb-btn.tb-on,
.titlebar .tb-btn.tb-pin-on { background: #e3f2ef; color: #0f5f5c; }
.titlebar .tb-btn.tb-close:hover:not(:disabled) { background: #c0392b; color: #fff; }
:root[data-theme="dark"] .titlebar { background: #162423; border-bottom-color: #2c403e; }
:root[data-theme="dark"] .titlebar .tb-name { color: #6fd0c4; }
:root[data-theme="dark"] .titlebar .tb-desc { color: #92aaa7; }
:root[data-theme="dark"] .titlebar .tb-ver { color: #73928f; }
:root[data-theme="dark"] .titlebar .tb-btn { color: #b9cfcc; }
:root[data-theme="dark"] .titlebar .tb-btn:hover:not(:disabled) { background: #203230; color: #6fd0c4; }
:root[data-theme="dark"] .titlebar .tb-btn.tb-on,
:root[data-theme="dark"] .titlebar .tb-btn.tb-pin-on { background: #1d3431; color: #6fd0c4; }
</style>
