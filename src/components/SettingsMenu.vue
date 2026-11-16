<script setup lang="ts">
/**
 * 设置图标的第一层交互：紧凑下拉菜单。
 * 点击普通菜单项 → emit open(key)，由 TitleBar 打开 Modal 并定位到对应页；
 * 「意见反馈」「帮助」是动作项，直接跳官方论坛对应板块（地址见 settings.ts）。
 */
import { onMounted, onUnmounted, ref } from "vue";
import { ExternalLink } from "@lucide/vue";
import { MENU_GROUPS, menuLabel, openFeedback, openForum, type MenuKey } from "./settings";

const emit = defineEmits<{
  open: [key: Exclude<MenuKey, "help">];
  close: [];
}>();

const rootEl = ref<HTMLElement | null>(null);
const notice = ref("");
let noticeTimer = 0;

function pick(m: MenuKey) {
  if (m === "help") {
    void onHelp();
    return;
  }
  if (m === "feedback") {
    void onFeedback();
    return;
  }
  emit("open", m);
  emit("close");
}

async function onHelp() {
  const msg = await openForum();
  if (msg) { showNotice(msg); return; }
  emit("close");
}
async function onFeedback() {
  const msg = await openFeedback();
  if (msg) { showNotice(msg); return; }
  emit("close");
}
function showNotice(text: string) {
  notice.value = text;
  window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => { notice.value = ""; }, 3200);
}

function onDocDown(e: MouseEvent) {
  const t = e.target as HTMLElement | null;
  if (!t || rootEl.value?.contains(t)) return;
  // 触发按钮自己负责 toggle，这里不能抢先关闭
  if (t.closest("[data-menu-trigger]")) return;
  emit("close");
}
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") emit("close");
}
onMounted(() => {
  document.addEventListener("mousedown", onDocDown);
  document.addEventListener("keydown", onKey);
});
onUnmounted(() => {
  document.removeEventListener("mousedown", onDocDown);
  document.removeEventListener("keydown", onKey);
  window.clearTimeout(noticeTimer);
});
</script>

<template>
  <div ref="rootEl" class="sm-root">
    <template v-for="(group, gi) in MENU_GROUPS" :key="gi">
      <div v-if="gi > 0" class="sm-sep" />
      <button
        v-for="m in group" :key="m.key"
        class="sm-item"
        :class="{ 'sm-action': m.action }"
        @click="pick(m.key)"
      >
        <component :is="m.icon" :size="15" :stroke-width="1.8" />
        <span>{{ menuLabel(m.key) }}</span>
        <ExternalLink v-if="m.action" :size="13" :stroke-width="1.7" class="sm-ext" />
      </button>
    </template>
    <div v-if="notice" class="sm-notice">{{ notice }}</div>
  </div>
</template>

<style scoped>
.sm-root {
  position: fixed;
  top: 46px;
  right: 92px; /* 右边缘对齐标题栏「设置」按钮 */
  z-index: 9999;
  width: 216px;
  padding: 6px;
  background: #fff;
  border: 1px solid #dce7e5;
  border-radius: 10px;
  box-shadow: 0 12px 32px rgba(20, 45, 43, .16), 0 2px 6px rgba(20, 45, 43, .07);
  animation: sm-in .14s ease-out;
  user-select: none;
}
@keyframes sm-in {
  from { opacity: 0; transform: translateY(-5px); }
  to { opacity: 1; transform: translateY(0); }
}
.sm-root .sm-item {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  padding: 9px 10px;
  margin: 0;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: #40514f;
  font-size: 13px;
  text-align: left;
  cursor: pointer;
}
.sm-root .sm-item:hover:not(:disabled) { background: #eef7f5; color: #0f5f5c; }
.sm-root .sm-item.sm-action { color: #0f5f5c; }
.sm-root .sm-ext { margin-left: auto; opacity: .6; }
.sm-root .sm-sep { height: 1px; margin: 5px 8px; background: #e4eeec; }
.sm-root .sm-notice {
  margin: 4px 4px 2px;
  padding: 7px 9px;
  border-radius: 6px;
  background: #eef7f5;
  border-left: 3px solid #0f5f5c;
  font-size: 11.5px;
  line-height: 1.5;
  color: #40514f;
}
:root[data-theme="dark"] .sm-root { background: #1b2a29; border-color: #2c403e; box-shadow: 0 12px 32px rgba(0, 0, 0, .5); }
:root[data-theme="dark"] .sm-root .sm-item { color: #c4d8d5; }
:root[data-theme="dark"] .sm-root .sm-item:hover:not(:disabled) { background: #203230; color: #6fd0c4; }
:root[data-theme="dark"] .sm-root .sm-item.sm-action { color: #6fd0c4; }
:root[data-theme="dark"] .sm-root .sm-sep { background: #2c403e; }
:root[data-theme="dark"] .sm-root .sm-notice { background: #1d3431; color: #b9cfcc; }
</style>
