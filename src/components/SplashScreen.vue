<script setup lang="ts">
/**
 * 启动页（Splash）：应用挂载后的短展示层，纯展示、不触碰任何业务初始化。
 *
 * - 图标与标题栏 / 桌面 / 任务栏共用同一份 public/grename-icon.svg；
 * - 亮暗配色跟随 <html data-theme>，与 theme.ts 及 App.vue 全局主题一致；
 * - 覆盖层首帧即完全不透明（不做整层淡入），窗口显示时不会透出主界面；
 *   logo 浮起与短进度条 1.15s 推进仍用 keyframes，不依赖 <Transition appear>；
 * - 离场由父级 <Transition name="splash"> 触发，见文件末尾的 leave 样式；
 * - 文案复用既有 i18n 键，不新增键、不改变双语键数基线。
 */
import { t } from "../i18n/index";
</script>

<template>
  <div class="splash" role="status" :aria-label="t('tb.desc')">
    <div class="splash-inner">
      <img class="splash-logo" src="/grename-icon.svg" alt="Grename" draggable="false" />
      <div class="splash-name">Grename</div>
      <div class="splash-sub">{{ t("tb.desc") }}</div>
      <div class="splash-track"><i class="splash-fill" /></div>
    </div>
  </div>
</template>

<style>
/* 类名统一 splash- 前缀；非 scoped，便于写 html[data-theme] 与 <Transition> 的全局类名 */
.splash {
  position: fixed;
  inset: 0;
  z-index: 9999;
  display: flex;
  align-items: center;
  justify-content: center;
  background: radial-gradient(circle at 50% 42%, #ffffff 0%, #eef6f4 58%, #e4f0ed 100%);
  /* 首帧必须不透明：窗口在 Vue 挂载后才显示，若从 opacity:0 淡入会先透出主界面 */
  opacity: 1;
  user-select: none;
  -webkit-user-select: none;
}
.splash-inner {
  display: flex;
  flex-direction: column;
  align-items: center;
  animation: splash-rise .62s cubic-bezier(.2, .8, .25, 1) both;
}
.splash-logo {
  width: 96px;
  height: 96px;
  object-fit: contain;
  filter: drop-shadow(0 10px 22px rgba(15, 95, 92, .22));
  animation: splash-pop .7s cubic-bezier(.2, .85, .3, 1) both;
}
.splash-name {
  margin-top: 18px;
  font-size: 26px;
  font-weight: 650;
  letter-spacing: .4px;
  color: #0f5f5c;
}
.splash-sub {
  margin-top: 4px;
  font-size: 12px;
  letter-spacing: 1.2px;
  color: #617473;
}
/* 短进度条：宽 180px、高 4px，填充从 0 平滑推进到 100%，末端带流光 */
.splash-track {
  width: 180px;
  height: 4px;
  margin-top: 30px;
  border-radius: 4px;
  background: #dceae8;
  overflow: hidden;
  animation: splash-track-in .5s .12s ease-out both;
}
.splash-fill {
  display: block;
  width: 0;
  height: 100%;
  border-radius: 4px;
  background: linear-gradient(90deg, #147570, #0f5f5c 55%, #4fb3a8);
  box-shadow: 0 0 8px rgba(20, 117, 112, .45);
  animation: splash-fill 1.15s .18s cubic-bezier(.35, .05, .2, 1) forwards;
}

@keyframes splash-rise { from { opacity: 0; transform: translateY(10px) } to { opacity: 1; transform: translateY(0) } }
@keyframes splash-pop { from { opacity: 0; transform: scale(.86) } to { opacity: 1; transform: scale(1) } }
@keyframes splash-track-in { from { opacity: 0; transform: scaleX(.7) } to { opacity: 1; transform: scaleX(1) } }
@keyframes splash-fill { from { width: 0 } to { width: 100% } }

/* 离场：父级 <Transition name="splash"> 加在根元素上，整层淡出并轻微放大 */
.splash-leave-active { transition: opacity .45s ease, transform .45s ease; }
.splash-leave-to { opacity: 0; transform: scale(1.025); }

/* 暗色主题：与 App.vue 的 html[data-theme="dark"] 同源 */
html[data-theme="dark"] .splash {
  background: radial-gradient(circle at 50% 42%, #1b2a29 0%, #162423 58%, #101c1b 100%);
}
html[data-theme="dark"] .splash-name { color: #6fd0c4; }
html[data-theme="dark"] .splash-sub { color: #92aaa7; }
html[data-theme="dark"] .splash-track { background: #2a3c3a; }
html[data-theme="dark"] .splash-fill {
  background: linear-gradient(90deg, #23857d, #6fd0c4 55%, #3d9089);
  box-shadow: 0 0 8px rgba(111, 208, 196, .35);
}

@media (prefers-reduced-motion: reduce) {
  .splash, .splash-inner, .splash-logo, .splash-track, .splash-fill { animation-duration: .01ms; }
  .splash-fill { width: 100%; }
  .splash-leave-active { transition-duration: .01ms; }
}
</style>
