<script setup lang="ts">
/**
 * 设置的第二层交互：Modal 设置对话框（历史文件名沿用，实际是居中对话框）。
 * 由下拉菜单项打开，props.initial 指定初始定位的内容页；左侧菜单可继续切换。
 *
 * 变化（本轮）：
 *  - 全部文案走 i18n，语言切换即时生效；
 *  - 「设置」页新增语言选择（数据来自 i18n 注册表，可自行扩展语言）；
 *  - 「设置」页新增日志文件位置（路径 + 打开 / 复制），底部不再展示日志行；
 *  - 接入后端 settings 表：保留期 / 复核令牌 TTL 从后端读取，主题 / 语言镜像到后端，
 *    「数据与维护」组提供 prune_history_cmd 的手动清理入口。
 *
 * 分工（v1.17 起）：外观仍以 localStorage 为准（首帧零延迟），保存或切换主题/语言时
 * 把完整快照写入后端 SQLite；保留期、TTL 与**默认递归 / 默认输出去向**以读到的后端值为准，
 * 因为它们要么是 Rust 运行时真正读取的数值，要么是主界面首帧要套用的默认行为。
 * 后端永久保留用 -1，界面「永久」沿用 0，只在边界做映射。
 * 后端 theme / locale 仍只作跨会话备份，不反向覆盖本机外观，避免首帧闪烁与老偏好被冲掉。
 *
 * v1.17 默认值后端化（M3 收尾 B）：这两项此前只写 localStorage、主界面从不读取，
 * 属于死配置。现在真正的读取方是 `App.vue` 的 `readLocalDefaults()`；本文件负责
 * 「后端覆盖本地」的合并与保存后广播 `grename:prefs-saved`，让主界面无需 props 也能刷新。
 */
import { computed, onMounted, onUnmounted, reactive, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  Check, Download, FileText, Heart, Maximize2, Monitor, Moon, Plus, QrCode, RefreshCw, Settings, Sun, Trash2, X,
} from "@lucide/vue";
import { MENU_GROUPS, menuLabel, pageTitle, openFeedback, openForum, type MenuKey, type PageKey } from "./settings";
import { getThemeMode, setTheme, type ThemeMode } from "../theme";
import { getLocaleOptions, locale, setLocale, t } from "../i18n/index";
import { APP_VERSION } from "../version";
import {
  compareSemver, fetchCommunity, fetchUpdate, formatNotes,
  mapFeedbackType, postFeedback,
  type DonationItem,
} from "../remote";
import { LOCAL_QRS, verifyQr } from "../qrVerify";

const props = defineProps<{ initial?: PageKey }>();
const emit = defineEmits<{ close: [] }>();

const active = ref<PageKey>(props.initial ?? "settings");
const currentTitle = computed(() => pageTitle(active.value));

interface Prefs {
  retention: number;       // 界面语义：0 = 永久；后端 -1 = 永久
  undoTtlSec: number;      // 撤销复核令牌有效期（秒）
  undoVerifyContent: boolean; // §7.1 可选内容指纹（默认关闭）
  recursive: boolean;
  outputMode: "in_place" | "to_dir";
  progressMs: number;
}
/** 后端 Settings，serde camelCase。 */
interface BackendSettings {
  historyRetentionDays: number;
  undoPrecheckTtlSec: number;
  undoVerifyContent: boolean;
  theme: string;
  locale: string;
  /** v1.17：设置里的「默认递归子目录」，主界面首帧与切任务前用它。 */
  defaultRecursive: boolean;
  /** v1.17：设置里的「默认输出去向」，只可能是 in_place / to_dir。 */
  defaultOutputMode: string;
}
const DEFAULTS: Prefs = {
  retention: 30, undoTtlSec: 300, undoVerifyContent: false, recursive: false,
  outputMode: "in_place", progressMs: 80,
};
const prefs = reactive<Prefs>({ ...DEFAULTS });
const saved = ref(false);
const logPath = ref("");
const updateMsg = ref("");
const showUpdateModal = ref(false);
const showQrModal = ref<{ key: string; labelKey: string } | null>(null);
const updateLink = ref("");
const updateNotes = ref("");
const copied = ref(false);
// 本地二维码经 SHA-256 校验通过才放进 verifiedQr；捐赠记录来自 /community
const verifiedQr = ref<Record<string, string>>({});
const donateRecords = ref<DonationItem[]>([]);
const donateLoaded = ref(false);
const helpNotice = ref("");
const backendReady = ref(false);
const maintMsg = ref("");
const maintErr = ref(false);
const cleaning = ref(false);
let savedTimer = 0;
let copiedTimer = 0;
let helpTimer = 0;
let maintTimer = 0;

const THEMES: { key: ThemeMode; labelKey: string; hintKey: string; icon: typeof Monitor }[] = [
  { key: "system", labelKey: "sd.theme.system", hintKey: "sd.theme.systemHint", icon: Monitor },
  { key: "dark", labelKey: "sd.theme.dark", hintKey: "sd.theme.darkHint", icon: Moon },
  { key: "light", labelKey: "sd.theme.light", hintKey: "sd.theme.lightHint", icon: Sun },
];
const themeMode = ref<ThemeMode>(getThemeMode());
const themeApplied = ref(false);
let themeTimer = 0;

const langOptions = getLocaleOptions();
const currentLocale = computed(() => locale.value);
const langApplied = ref(false);
let langTimer = 0;

function chooseTheme(m: ThemeMode) {
  themeMode.value = m;
  setTheme(m); // 立即生效 + 持久化
  themeApplied.value = true;
  window.clearTimeout(themeTimer);
  themeTimer = window.setTimeout(() => { themeApplied.value = false; }, 1800);
  if (backendReady.value) void syncBackend(false);
}
function onLocaleChange(e: Event) {
  const code = (e.target as HTMLSelectElement).value;
  setLocale(code); // 即时生效 + 持久化
  langApplied.value = true;
  window.clearTimeout(langTimer);
  langTimer = window.setTimeout(() => { langApplied.value = false; }, 1800);
  if (backendReady.value) void syncBackend(false);
}

function loadPrefs() {
  try {
    const raw = localStorage.getItem("grename.settings");
    if (raw) Object.assign(prefs, JSON.parse(raw) as Partial<Prefs>);
  } catch { /* 本地偏好损坏时忽略，使用默认值 */ }
}
/** 只写本地并广播，不触发后端回写（供 loadBackend 合并时用）。 */
function writeLocalPrefs() {
  try { localStorage.setItem("grename.settings", JSON.stringify({ ...prefs })); } catch { /* 存储不可用时忽略 */ }
  // 主界面不通过 props 接收偏好，靠这个事件重新读一次快照即可，避免加一层状态管理。
  window.dispatchEvent(new CustomEvent("grename:prefs-saved"));
}
function savePrefs() {
  writeLocalPrefs();
  saved.value = true;
  window.clearTimeout(savedTimer);
  savedTimer = window.setTimeout(() => { saved.value = false; }, 1800);
  void syncBackend(true);
}
function showMaint(msg: string, isErr = false) {
  maintMsg.value = msg;
  maintErr.value = isErr;
  window.clearTimeout(maintTimer);
  maintTimer = window.setTimeout(() => { maintMsg.value = ""; }, 3600);
}
/** 界面值 → 后端值。0（永久）映射为 -1，其余天数原样。 */
function composeBackend(): BackendSettings {
  return {
    historyRetentionDays: prefs.retention === 0 ? -1 : prefs.retention,
    undoPrecheckTtlSec: prefs.undoTtlSec,
    undoVerifyContent: prefs.undoVerifyContent,
    theme: themeMode.value,
    locale: locale.value,
    defaultRecursive: prefs.recursive,
    defaultOutputMode: prefs.outputMode,
  };
}
async function loadBackend() {
  try {
    const s = await invoke<BackendSettings>("get_settings");
    prefs.retention = s.historyRetentionDays < 0 ? 0 : s.historyRetentionDays;
    prefs.undoTtlSec = s.undoPrecheckTtlSec;
    prefs.undoVerifyContent = s.undoVerifyContent;
    // v1.17：默认值以后端为准（用户已确认「后端覆盖本地」），但要区分「后端确实设过」
    // 与「老库升级后读到的是出厂值」——后者若直接覆盖，会把用户此前的本地偏好冲掉。
    const backendIsDefault =
      s.defaultRecursive === DEFAULTS.recursive
      && s.defaultOutputMode === DEFAULTS.outputMode;
    if (backendIsDefault) {
      const localNonDefault =
        prefs.recursive !== DEFAULTS.recursive
        || prefs.outputMode !== DEFAULTS.outputMode;
      if (localNonDefault) void syncBackend(false); // 老库升级：把本地偏好补写进后端
    } else {
      prefs.recursive = s.defaultRecursive === true;
      prefs.outputMode = s.defaultOutputMode === "to_dir" ? "to_dir" : "in_place";
      writeLocalPrefs(); // 同步本地快照，让主界面与后端一致
    }
  } catch (e) {
    showMaint(t("sd.maint.loadFailed", { err: String(e) }), true);
  } finally {
    backendReady.value = true;
  }
}
/** 把完整快照写入 SQLite；showOk=false 用于主题/语言切换时的静默镜像。 */
async function syncBackend(showOk: boolean) {
  try {
    await invoke<BackendSettings>("set_settings", { settings: composeBackend() });
    if (showOk) showMaint(t("sd.maint.saved"));
  } catch (e) {
    showMaint(t("sd.maint.saveFailed", { err: String(e) }), true);
  }
}
async function cleanNow() {
  cleaning.value = true;
  try {
    const n = await invoke<number>("prune_history_cmd");
    showMaint(n > 0 ? t("sd.maint.cleanDone", { n }) : t("sd.maint.cleanNone"));
  } catch (e) {
    showMaint(t("sd.maint.cleanFailed", { err: String(e) }), true);
  } finally { cleaning.value = false; }
}
function resetPrefs() {
  Object.assign(prefs, DEFAULTS); // 主题 / 语言独立存储，不在此重置
  savePrefs();
}
async function openLog() {
  try { await invoke("open_log_file"); } catch { /* 由主界面提示 */ }
}
async function copyText(text: string) {
  try {
    await navigator.clipboard.writeText(text);
    copied.value = true;
    window.clearTimeout(copiedTimer);
    copiedTimer = window.setTimeout(() => { copied.value = false; }, 1800);
  } catch { /* 剪贴板不可用时忽略 */ }
}
// ---------------------------------------------------------------- 意见反馈表单
/** 截图附件：上传接口待接入，当前只在本地做数量 / 大小 / 类型校验与预览。 */
interface FeedbackShot { id: number; name: string; url: string; sizeText: string }
const MAX_SHOTS = 5;
const MAX_SHOT_BYTES = 500 * 1024;
const FEEDBACK_TYPES = [
  { value: "other", labelKey: "fb.typeOther" },
  { value: "suggestion", labelKey: "fb.typeSuggest" },
  { value: "bug", labelKey: "fb.typeBug" },
];
const fbForm = reactive({ name: "", type: "other", contact: "", content: "" });
const fbShots = ref<FeedbackShot[]>([]);
const fbShotErr = ref("");
const fbMsg = ref("");
const fbMsgErr = ref(false);
const shotInput = ref<HTMLInputElement | null>(null);
let shotSeq = 0;
let fbMsgTimer = 0;

/** 点击方形「+」块，触发隐藏的文件选择框。 */
function pickShots() {
  shotInput.value?.click();
}

function onShotsPicked(e: Event) {
  const input = e.target as HTMLInputElement;
  const files = Array.from(input.files ?? []);
  input.value = ""; // 允许重复选择同一文件
  if (!files.length) return;
  const room = MAX_SHOTS - fbShots.value.length;
  if (room <= 0) { fbShotErr.value = t("fb.shotsFull", { n: MAX_SHOTS }); return; }
  let err = files.length > room ? t("fb.shotsFull", { n: MAX_SHOTS }) : "";
  for (const f of files.slice(0, room)) {
    if (!f.type.startsWith("image/")) { err = err || t("fb.shotNotImage", { name: f.name }); continue; }
    if (f.size > MAX_SHOT_BYTES) { err = err || t("fb.shotTooLarge", { name: f.name }); continue; }
    fbShots.value.push({
      id: ++shotSeq, name: f.name,
      url: URL.createObjectURL(f),
      sizeText: `${Math.max(1, Math.round(f.size / 1024))} KB`,
    });
  }
  fbShotErr.value = err;
}
function removeShot(id: number) {
  const i = fbShots.value.findIndex((s) => s.id === id);
  if (i >= 0) {
    URL.revokeObjectURL(fbShots.value[i].url);
    fbShots.value.splice(i, 1);
  }
  fbShotErr.value = "";
}
function showFb(msg: string, isErr: boolean) {
  fbMsg.value = msg;
  fbMsgErr.value = isErr;
  window.clearTimeout(fbMsgTimer);
  fbMsgTimer = window.setTimeout(() => { fbMsg.value = ""; }, 4200);
}
const submittingFb = ref(false);
async function submitFeedback() {
  if (!fbForm.name) { showFb(t("fb.needName"), true); return; }
  if (!fbForm.content) { showFb(t("fb.needContent"), true); return; }
  if (submittingFb.value) return;
  submittingFb.value = true;
  try {
    // 只提交文本字段；截图上传接口后端暂未提供，仍只在本地预览（见 fb.pending 文案）。
    const res = await postFeedback({
      name: fbForm.name,
      type: mapFeedbackType(fbForm.type),
      contact: fbForm.contact,
      content: fbForm.content,
    });
    showFb(res.message || t("fb.sent"), !res.ok);
    if (res.ok) {
      fbForm.name = "";
      fbForm.contact = "";
      fbForm.content = "";
    }
  } catch (e) {
    showFb(t("fb.failed", { err: e instanceof Error ? e.message : String(e) }), true);
  } finally {
    submittingFb.value = false;
  }
}

// ---------------------------------------------------------------- 捐赠页渠道（真码由 /community 校验后填充）
/** 二维码键与后端 qrs.platform 对齐；labelKey 走 i18n，保持语言切换响应。 */
const DONATE_CHANNELS = [
  { key: "wechat", platform: "wechat", labelKey: "dn.wechat" },
  { key: "alipay", platform: "alipay", labelKey: "dn.alipay" },
];

/** /community 只提供捐赠记录；二维码已改为本地离线校验，不再消费后端 qrs。 */
async function loadCommunity() {
  try {
    const data = await fetchCommunity();
    donateRecords.value = Array.isArray(data.donations) ? data.donations : [];
  } catch {
    donateRecords.value = [];
  } finally {
    donateLoaded.value = true;
  }
}

/** 本地二维码：只读取随应用打包的图片并做 SHA-256 比对，不请求任何远程资源。 */
async function verifyLocalQrs() {
  const checked: Record<string, string> = {};
  await Promise.all(Object.entries(LOCAL_QRS).map(async ([platform, q]) => {
    try {
      if (await verifyQr(q.url, q.sha256)) checked[platform] = q.url;
    } catch { /* 校验失败：保持占位，绝不展示未通过校验的码 */ }
  }));
  verifiedQr.value = checked;
}

// ---------------------------------------------------------------- 关于页作者信息
/** 官网 / 公众号（博客与邮箱已按需求移除；公众号图来自本地 imgs/，哈希校验通过才显示）。 */
const ABOUT_LINKS = {
  website: "https://gr.graent.cn/grename",
  wechatName: "极志猿",
};
const aboutLinks = ABOUT_LINKS;
// 关于页公众号二维码弹窗开关
const showWxQr = ref(false);
async function openLink(url: string) {
  try { await openUrl(url); } catch { /* 非 Tauri 环境 / 无可用程序：静默 */ }
}

const checkingUpdate = ref(false);
async function checkUpdate() {
  if (checkingUpdate.value) return;
  checkingUpdate.value = true;
  showUpdateModal.value = false;
  updateLink.value = "";
  updateNotes.value = "";
  try {
    const info = await fetchUpdate();
    if (!info.latest) {
      // 后端尚未发布版本（latest 为空）：如实告知，不伪造更新
      updateMsg.value = t("sd.update.none");
    } else if (compareSemver(info.latest, APP_VERSION) > 0) {
      updateMsg.value = t("sd.update.available", { v: info.latest, cur: APP_VERSION });
      updateLink.value = info.url || "";
      updateNotes.value = formatNotes(info.notes);
    } else {
      updateMsg.value = t("sd.update.current", { v: APP_VERSION, latest: info.latest });
    }
  } catch (e) {
    updateMsg.value = t("sd.update.failed", { err: e instanceof Error ? e.message : String(e) });
  } finally {
    checkingUpdate.value = false;
    showUpdateModal.value = true; // 结果统一用 Modal 弹出
  }
}
async function onMenu(m: MenuKey) {
  if (m !== "help" && m !== "feedback") { active.value = m as PageKey; return; }
  helpNotice.value = m === "help" ? await openForum() : await openFeedback();
  window.clearTimeout(helpTimer);
  if (helpNotice.value) {
    helpTimer = window.setTimeout(() => { helpNotice.value = ""; }, 3600);
  }
}
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") emit("close");
}
onMounted(() => {
  loadPrefs();
  void loadBackend();
  void loadCommunity(); // 只拉捐赠记录；失败保持空态，不影响界面
  void verifyLocalQrs(); // 本地二维码哈希校验，完全离线，不依赖网络
  document.addEventListener("keydown", onKey);
  void invoke<string>("get_log_path")
    .then((p) => { logPath.value = p; })
    .catch(() => { logPath.value = ""; });
});
onUnmounted(() => {
  document.removeEventListener("keydown", onKey);
  window.clearTimeout(savedTimer);
  window.clearTimeout(copiedTimer);
  window.clearTimeout(helpTimer);
  window.clearTimeout(themeTimer);
  window.clearTimeout(langTimer);
  window.clearTimeout(maintTimer);
  window.clearTimeout(fbMsgTimer);
  for (const s of fbShots.value) URL.revokeObjectURL(s.url);
});
</script>

<template>
  <div class="sd-mask" @mousedown.self="emit('close')">
    <div class="sd-dialog" role="dialog" aria-modal="true" :aria-label="currentTitle">
      <div class="sd-head">
        <div class="sd-head-left">
          <span class="sd-head-icon"><Settings :size="16" :stroke-width="1.8" /></span>
          <b>{{ t("sd.title") }}</b>
        </div>
        <button class="sd-x" :title="t('sd.close')" :aria-label="t('sd.close')" @click="emit('close')">
          <X :size="16" :stroke-width="1.8" />
        </button>
      </div>

      <div class="sd-cols">
        <nav class="sd-side">
          <template v-for="(group, gi) in MENU_GROUPS" :key="gi">
            <div v-if="gi > 0" class="sd-sep" />
            <button
              v-for="m in group" :key="m.key"
              class="sd-item"
              :class="{ 'sd-active': active === m.key, 'sd-action': m.action }"
              @click="onMenu(m.key)"
            >
              <component :is="m.icon" :size="15" :stroke-width="1.8" />
              <span>{{ menuLabel(m.key) }}</span>
            </button>
          </template>
          <p v-if="helpNotice" class="sd-help-note">{{ helpNotice }}</p>
          <div class="sd-side-foot">v{{ APP_VERSION }}</div>
        </nav>

        <div class="sd-main">
          <div class="sd-page-head">
            <h3>{{ currentTitle }}</h3>
            <span>{{ active === "settings" ? t("sd.pageSubSettings") : t("sd.pageSub") }}</span>
          </div>

          <div class="sd-body scroller">
            <!-- 设置 -->
            <div v-if="active === 'settings'" class="sd-page">
              <div class="sd-group">{{ t("sd.group.language") }}</div>
              <label class="sd-field">
                <span>{{ t("sd.lang.field") }}</span>
                <select :value="currentLocale" @change="onLocaleChange">
                  <option v-for="o in langOptions" :key="o.code" :value="o.code">{{ o.label }}</option>
                </select>
              </label>
              <p class="sd-note sd-lang-note">
                {{ t("sd.lang.hint") }}
                <span v-if="langApplied" class="sd-ok"><Check :size="12" /> {{ t("sd.lang.applied") }}</span>
              </p>

              <div class="sd-group">{{ t("sd.group.theme") }}</div>
              <div class="sd-themes">
                <button
                  v-for="x in THEMES" :key="x.key"
                  class="sd-theme" :class="{ 'sd-theme-on': themeMode === x.key }"
                  :data-k="x.key"
                  @click="chooseTheme(x.key)"
                >
                  <span class="sd-theme-top">
                    <span class="sd-theme-icon"><component :is="x.icon" :size="17" :stroke-width="1.8" /></span>
                    <Check v-if="themeMode === x.key" :size="14" :stroke-width="2.2" class="sd-theme-check" />
                  </span>
                  <b>{{ t(x.labelKey) }}</b>
                  <small>{{ t(x.hintKey) }}</small>
                  <span class="sd-sw"><i /><i /><i /></span>
                </button>
              </div>
              <p class="sd-note sd-theme-note">
                {{ t("sd.theme.note") }}
                <span v-if="themeApplied" class="sd-ok"><Check :size="12" /> {{ t("sd.theme.applied") }}</span>
              </p>

              <div class="sd-group">{{ t("sd.group.general") }}</div>
              <label class="sd-field">
                <span>{{ t("sd.field.retention") }}</span>
                <select v-model.number="prefs.retention">
                  <option :value="7">{{ t("sd.opt.days7") }}</option>
                  <option :value="30">{{ t("sd.opt.days30") }}</option>
                  <option :value="90">{{ t("sd.opt.days90") }}</option>
                  <option :value="0">{{ t("sd.opt.forever") }}</option>
                </select>
              </label>
              <label class="sd-field">
                <span>{{ t("sd.field.progressMs") }}</span>
                <select v-model.number="prefs.progressMs">
                  <option :value="50">50 ms</option>
                  <option :value="80">{{ t("sd.opt.msDefault", { n: 80 }) }}</option>
                  <option :value="100">100 ms</option>
                  <option :value="200">200 ms</option>
                </select>
              </label>
              <label class="sd-field">
                <span>{{ t("sd.field.undoTtl") }}</span>
                <select v-model.number="prefs.undoTtlSec">
                  <option :value="60">{{ t("sd.opt.ttl1m") }}</option>
                  <option :value="300">{{ t("sd.opt.ttl5m") }}</option>
                  <option :value="900">{{ t("sd.opt.ttl15m") }}</option>
                  <option :value="3600">{{ t("sd.opt.ttl1h") }}</option>
                </select>
              </label>
              <label class="sd-field">
                <span>{{ t("sd.field.verifyContent") }}</span>
                <input type="checkbox" v-model="prefs.undoVerifyContent" />
              </label>
              <p class="sd-note sd-maint-hint">{{ t("sd.note.verifyContent") }}</p>
              <div class="sd-group">{{ t("sd.group.defaults") }}</div>
              <label class="sd-field">
                <span>{{ t("sd.field.recursive") }}</span>
                <input type="checkbox" v-model="prefs.recursive" />
              </label>
              <label class="sd-field">
                <span>{{ t("sd.field.outputMode") }}</span>
                <select v-model="prefs.outputMode">
                  <option value="in_place">{{ t("sd.opt.inPlace") }}</option>
                  <option value="to_dir">{{ t("sd.opt.toDir") }}</option>
                </select>
              </label>
              <p class="sd-note sd-maint-hint">{{ t("sd.note.defaults") }}</p>
              <div class="sd-btns">
                <button class="sd-btn" @click="savePrefs">{{ t("sd.btn.save") }}</button>
                <button class="sd-btn sd-btn-ghost" @click="resetPrefs">{{ t("sd.btn.reset") }}</button>
                <span v-if="saved" class="sd-ok"><Check :size="13" /> {{ t("sd.saved") }}</span>
              </div>

              <div class="sd-group">{{ t("sd.group.maintenance") }}</div>
              <p class="sd-note sd-maint-hint">{{ t("sd.maint.hint") }}</p>
              <div class="sd-btns">
                <button class="sd-btn sd-btn-ghost" :disabled="!backendReady || cleaning" @click="cleanNow">
                  <Trash2 :size="14" :stroke-width="1.8" /> {{ t("sd.btn.cleanNow") }}
                </button>
                <span v-if="maintMsg" class="sd-maint-msg" :class="{ err: maintErr }">{{ maintMsg }}</span>
                <span v-else-if="!backendReady" class="sd-note">{{ t("sd.maint.loading") }}</span>
              </div>

              <div class="sd-group">{{ t("sd.group.log") }}</div>
              <div class="sd-kv">
                <span>{{ t("sd.log.location") }}</span>
                <code class="sd-code" :title="logPath">{{ logPath || t("sd.log.reading") }}</code>
              </div>
              <div class="sd-btns">
                <button class="sd-btn sd-btn-ghost" @click="openLog">
                  <FileText :size="14" :stroke-width="1.8" /> {{ t("sd.btn.openLog") }}
                </button>
                <button class="sd-btn sd-btn-ghost" @click="copyText(logPath)">{{ t("sd.btn.copyLog") }}</button>
                <span v-if="copied" class="sd-ok"><Check :size="13" /> {{ t("sd.copied") }}</span>
              </div>
            </div>

            <!-- 意见反馈 -->
            <div v-else-if="active === 'feedback'" class="sd-page">
              <div class="sd-group">{{ t("sd.feedback.title") }}</div>
              <p class="sd-text sd-form-intro">{{ t("sd.feedback.text") }}</p>

              <form class="sd-form" @submit.prevent="submitFeedback">
                <label class="sd-frow">
                  <span class="sd-flabel">{{ t("fb.name") }}<i>*</i></span>
                  <input v-model.trim="fbForm.name" type="text" maxlength="40" :placeholder="t('fb.namePh')" />
                </label>
                <label class="sd-frow">
                  <span class="sd-flabel">{{ t("fb.type") }}<i>*</i></span>
                  <select v-model="fbForm.type">
                    <option v-for="o in FEEDBACK_TYPES" :key="o.value" :value="o.value">{{ t(o.labelKey) }}</option>
                  </select>
                </label>
                <label class="sd-frow">
                  <span class="sd-flabel">{{ t("fb.contact") }}<em>{{ t("fb.optional") }}</em></span>
                  <input v-model.trim="fbForm.contact" type="text" maxlength="80" :placeholder="t('fb.contactPh')" />
                </label>
                <div class="sd-frow sd-frow-col">
                  <label class="sd-flabel" for="fb-content">{{ t("fb.content") }}<i>*</i></label>
                  <textarea id="fb-content" v-model.trim="fbForm.content" rows="5" maxlength="2000" :placeholder="t('fb.contentPh')"></textarea>
                  <small class="sd-fcount">{{ fbForm.content.length }} / 2000</small>
                </div>
                <div class="sd-frow sd-frow-col">
                  <span class="sd-flabel">{{ t("fb.shots") }}<em>{{ t("fb.shotsHint") }}</em></span>
                  <div class="sd-shots">
                    <div v-for="s in fbShots" :key="s.id" class="sd-shot" :title="s.name + ' · ' + s.sizeText">
                      <img :src="s.url" :alt="s.name" />
                      <button type="button" class="sd-shot-x" :aria-label="t('fb.remove')" @click="removeShot(s.id)">×</button>
                    </div>
                    <button
                      v-if="fbShots.length < MAX_SHOTS"
                      type="button"
                      class="sd-shot sd-shot-add"
                      :title="t('fb.shots')"
                      :aria-label="t('fb.shots')"
                      @click="pickShots"
                    >
                      <Plus :size="20" :stroke-width="1.8" />
                    </button>
                  </div>
                  <input ref="shotInput" class="sd-file-hidden" type="file" accept="image/*" multiple @change="onShotsPicked" />
                  <p v-if="fbShotErr" class="sd-ferr">{{ fbShotErr }}</p>
                </div>
                <div class="sd-btns">
                  <button class="sd-btn" type="submit">{{ t("fb.submit") }}</button>
                  <span v-if="fbMsg" class="sd-maint-msg" :class="{ err: fbMsgErr }">{{ fbMsg }}</span>
                </div>
              </form>
            </div>

            <!-- 捐赠：记录来自 /community，二维码为本地资源且经 SHA-256 校验通过才显示 -->
            <div v-else-if="active === 'donate'" class="sd-page">
              <div class="sd-group dn-title"><Heart :size="15" :stroke-width="1.8" /> {{ t("dn.title") }}</div>
              <p class="sd-text dn-intro">{{ t("dn.intro") }}</p>
              <div class="dn-qrs">
                <div v-for="q in DONATE_CHANNELS" :key="q.key" class="dn-qr">
                  <button type="button" class="dn-qr-box" :aria-label="t(q.labelKey)" @click="showQrModal = q">
                    <img v-if="verifiedQr[q.platform]" class="dn-qr-real" :src="verifiedQr[q.platform]" :alt="t(q.labelKey)" />
                    <svg v-else class="dn-qr-ph" viewBox="0 0 100 100" aria-hidden="true">
                      <defs>
                        <pattern :id="'dnGrid-' + q.key" width="10" height="10" patternUnits="userSpaceOnUse">
                          <rect width="10" height="10" fill="#eef4f3" />
                          <rect x="1" y="1" width="4" height="4" fill="#c9dcd9" />
                          <rect x="6" y="6" width="3" height="3" fill="#b9d2ce" />
                        </pattern>
                      </defs>
                      <rect x="0" y="0" width="100" height="100" :fill="'url(#dnGrid-' + q.key + ')'" />
                      <rect x="6" y="6" width="24" height="24" fill="none" stroke="#9dbcb8" stroke-width="4" />
                      <rect x="70" y="6" width="24" height="24" fill="none" stroke="#9dbcb8" stroke-width="4" />
                      <rect x="6" y="70" width="24" height="24" fill="none" stroke="#9dbcb8" stroke-width="4" />
                    </svg>
                    <span v-if="!verifiedQr[q.platform]" class="dn-qr-badge">{{ t("dn.placeholder") }}</span>
                    <span class="dn-qr-zoom"><Maximize2 :size="14" :stroke-width="1.9" /></span>
                  </button>
                  <b>{{ t(q.labelKey) }}</b>
                </div>
              </div>

              <div class="sd-group">{{ t("dn.records") }}<span class="dn-sample">{{ t("dn.sample") }}</span></div>
              <table class="dn-table">
                <thead>
                  <tr>
                    <th>{{ t("dn.colDate") }}</th>
                    <th>{{ t("dn.colName") }}</th>
                    <th>{{ t("dn.colAmount") }}</th>
                    <th>{{ t("dn.colNote") }}</th>
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="(r, i) in donateRecords" :key="i">
                    <td>{{ r.date }}</td>
                    <td>{{ r.name }}</td>
                    <td>¥{{ r.amount }}</td>
                    <td>{{ r.message }}</td>
                  </tr>
                  <tr v-if="!donateRecords.length">
                    <td colspan="4" class="dn-empty">{{ t("dn.empty") }}</td>
                  </tr>
                </tbody>
              </table>
            </div>

            <!-- 检查更新 -->
            <div v-else-if="active === 'update'" class="sd-page">
              <div class="sd-group">{{ t("sd.update.group") }}</div>
              <div class="sd-kv sd-kv-update">
                <span>{{ t("sd.update.version") }}</span>
                <div class="sd-update-tail">
                  <b>{{ APP_VERSION }}</b>
                  <!-- 检查更新 URL 稍后接入；当前仍是本地版本展示，不发起网络请求 -->
                  <button class="sd-btn sd-btn-sm" :disabled="checkingUpdate" @click="checkUpdate">
                    <RefreshCw :size="13" :stroke-width="1.8" :class="{ 'sd-spin': checkingUpdate }" /> {{ t("sd.update.check") }}
                  </button>
                </div>
              </div>
            </div>

            <!-- 关于 -->
            <div v-else class="sd-page">
              <div class="sd-about-head">
                <span class="sd-about-logo">G</span>
                <div><b>Grename</b><small>{{ t("sd.about.name") }}</small></div>
              </div>
              <div class="sd-kv"><span>{{ t("sd.about.version") }}</span><b>{{ APP_VERSION }}</b></div>
              <div class="sd-group sd-about-author-title">{{ t("sd.about.author") }}</div>
              <div class="sd-kv"><span>{{ t("sd.about.website") }}</span>
                <a class="sd-link" :href="aboutLinks.website" @click.prevent="openLink(aboutLinks.website)">{{ aboutLinks.website }}</a>
              </div>
              <div class="sd-kv sd-kv-author">
                <span>{{ t("sd.about.wechat") }}</span>
                <div class="sd-author-wx">
                  <span class="sd-author-wx-name">{{ aboutLinks.wechatName }}</span>
                  <button class="sd-wx-qr-btn" type="button" :title="t('sd.about.wechat')" :aria-label="t('sd.about.wechat')" @click="showWxQr = true">
                    <QrCode :size="15" :stroke-width="1.8" />
                  </button>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
      <!-- 公众号二维码弹窗：本地图经 SHA-256 校验通过才显示，否则保持占位 -->
      <div v-if="showWxQr" class="sd-wx-qr-mask" @mousedown.self="showWxQr = false">
        <div class="sd-wx-qr-dialog sd-qr-mp-dialog" role="dialog" aria-modal="true">
          <div class="sd-wx-qr-head">
            <b>{{ t("sd.about.wechat") }}</b>
            <button class="sd-x" type="button" :title="t('sd.close')" :aria-label="t('sd.close')" @click="showWxQr = false"><X :size="16" :stroke-width="1.8" /></button>
          </div>
          <div class="sd-wx-qr-body">
            <div v-if="verifiedQr.mp" class="sd-qr-mp-box">
              <img class="dn-qr-real" :src="verifiedQr.mp" :alt="t('sd.about.wechat')" />
            </div>
            <div v-else class="sd-qr-mp-ph"><span class="sd-wx-qr-ph-mark">{{ t("sd.about.wechatPh") }}</span></div>
            <p class="sd-mp-tip">{{ t("sd.about.wechatSearch") }}</p>
          </div>
        </div>
      </div>

      <!-- 检查更新结果弹窗：来自 /grename/update 的真实结果（含下载链接） -->
      <div v-if="showUpdateModal" class="sd-wx-qr-mask" @mousedown.self="showUpdateModal = false">
        <div class="sd-wx-qr-dialog" role="dialog" aria-modal="true">
          <div class="sd-wx-qr-head">
            <b>{{ t("sd.update.group") }}</b>
            <button class="sd-x" type="button" :title="t('sd.close')" :aria-label="t('sd.close')" @click="showUpdateModal = false"><X :size="16" :stroke-width="1.8" /></button>
          </div>
          <div class="sd-update-modal-body">
            <div class="sd-update-modal-badge"><RefreshCw :size="22" :stroke-width="1.6" /></div>
            <p class="sd-update-modal-msg">{{ updateMsg }}</p>
            <pre v-if="updateNotes" class="sd-update-notes">{{ updateNotes }}</pre>
            <button v-if="updateLink" class="sd-btn sd-update-download" type="button" @click="openLink(updateLink)">
              <Download :size="14" :stroke-width="1.8" /> {{ t("sd.update.download") }}
            </button>
          </div>
        </div>
      </div>

      <!-- 捐赠二维码放大弹窗：本地码经 SHA-256 校验通过才显示，否则保持占位 -->
      <div v-if="showQrModal" class="sd-wx-qr-mask" @mousedown.self="showQrModal = null">
        <div class="sd-wx-qr-dialog sd-qr-big-dialog" role="dialog" aria-modal="true">
          <div class="sd-wx-qr-head">
            <b>{{ t(showQrModal.labelKey) }}</b>
            <button class="sd-x" type="button" :title="t('sd.close')" :aria-label="t('sd.close')" @click="showQrModal = null"><X :size="16" :stroke-width="1.8" /></button>
          </div>
          <div class="sd-wx-qr-body">
            <div class="sd-qr-big-box">
              <img v-if="verifiedQr[showQrModal.key === 'wechat' ? 'wechat' : 'alipay']" class="dn-qr-real"
                   :src="verifiedQr[showQrModal.key === 'wechat' ? 'wechat' : 'alipay']" :alt="t(showQrModal.labelKey)" />
              <template v-else>
                <svg class="dn-qr-ph" viewBox="0 0 100 100" aria-hidden="true">
                  <defs>
                    <pattern :id="'dnBig-' + showQrModal.key" width="10" height="10" patternUnits="userSpaceOnUse">
                      <rect width="10" height="10" fill="#eef4f3" />
                      <rect x="1" y="1" width="4" height="4" fill="#c9dcd9" />
                      <rect x="6" y="6" width="3" height="3" fill="#b9d2ce" />
                    </pattern>
                  </defs>
                  <rect x="0" y="0" width="100" height="100" :fill="'url(#dnBig-' + showQrModal.key + ')'" />
                  <rect x="6" y="6" width="24" height="24" fill="none" stroke="#9dbcb8" stroke-width="4" />
                  <rect x="70" y="6" width="24" height="24" fill="none" stroke="#9dbcb8" stroke-width="4" />
                  <rect x="6" y="70" width="24" height="24" fill="none" stroke="#9dbcb8" stroke-width="4" />
                </svg>
                <span class="dn-qr-badge">{{ t("dn.placeholder") }}</span>
              </template>
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.sd-mask {
  position: fixed;
  inset: 0;
  z-index: 10000;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(12, 32, 30, .42);
  animation: sd-fade .14s ease-out;
}
@keyframes sd-fade { from { opacity: 0; } to { opacity: 1; } }
.sd-mask .sd-dialog {
  display: flex;
  flex-direction: column;
  width: 720px;
  height: 524px;
  max-width: calc(100vw - 40px);
  max-height: calc(100vh - 60px);
  overflow: hidden;
  background: #fff;
  border: 1px solid #d5e5e2;
  border-radius: 12px;
  box-shadow: 0 24px 64px rgba(10, 34, 31, .28), 0 4px 12px rgba(10, 34, 31, .10);
  font-size: 13px;
  animation: sd-rise .16s ease-out;
}
@keyframes sd-rise { from { opacity: 0; transform: translateY(8px) scale(.99); } to { opacity: 1; transform: none; } }

.sd-mask .sd-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  flex: 0 0 auto;
  height: 50px;
  padding: 0 14px 0 16px;
  background: #f7fbfa;
  border-bottom: 1px solid #e4eeec;
}
.sd-mask .sd-head-left { display: flex; align-items: center; gap: 9px; }
.sd-mask .sd-head-icon {
  display: inline-flex; align-items: center; justify-content: center;
  width: 26px; height: 26px; border-radius: 7px;
  background: #0f5f5c; color: #fff;
}
.sd-mask .sd-head b { font-size: 14px; font-weight: 600; color: #0f5f5c; }
.sd-mask .sd-head-sub { font-size: 11px; color: #91a3a1; margin-left: 4px; }
.sd-mask .sd-x {
  display: flex; align-items: center; justify-content: center;
  width: 28px; height: 28px; padding: 0;
  border: 0; border-radius: 6px; background: transparent; color: #718684; cursor: pointer;
}
.sd-mask .sd-x:hover:not(:disabled) { background: #e8f2f0; color: #0f5f5c; }

.sd-mask .sd-cols { display: flex; flex: 1; min-height: 0; }
.sd-mask .sd-side {
  display: flex; flex-direction: column;
  width: 164px; flex: 0 0 164px;
  padding: 12px 8px;
  background: #fbfdfc;
  border-right: 1px solid #e9f2f0;
}
.sd-mask .sd-item {
  display: flex; align-items: center; gap: 9px;
  width: 100%; padding: 9px 10px; margin-bottom: 2px;
  border: 0; border-radius: 6px; background: transparent;
  color: #40514f; font-size: 13px; text-align: left; cursor: pointer;
}
.sd-mask .sd-item:hover:not(:disabled) { background: #e9f4f2; color: #0f5f5c; }
.sd-mask .sd-item.sd-active { background: #0f5f5c; color: #fff; }
.sd-mask .sd-item.sd-active:hover:not(:disabled) { background: #147570; color: #fff; }
.sd-mask .sd-item.sd-action { color: #0f5f5c; }
.sd-mask .sd-sep { height: 1px; margin: 8px 10px; background: #dce7e5; }
.sd-mask .sd-help-note {
  margin: 6px 4px 0; padding: 7px 8px; border-radius: 6px;
  background: #eef7f5; border-left: 3px solid #0f5f5c;
  font-size: 11px; line-height: 1.5; color: #40514f;
}
.sd-mask .sd-side-foot { margin-top: auto; padding: 0 10px 2px; font-size: 11px; color: #a8bdb9; }

.sd-mask .sd-main { display: flex; flex-direction: column; flex: 1; min-width: 0; }
.sd-mask .sd-page-head {
  display: flex; align-items: baseline; justify-content: space-between;
  flex: 0 0 auto; padding: 14px 20px 10px; border-bottom: 1px solid #eef3f2;
}
.sd-mask .sd-page-head h3 { margin: 0; font-size: 15px; font-weight: 600; color: #0f5f5c; }
.sd-mask .sd-page-head span { font-size: 11.5px; color: #a0b3b0; }
.sd-mask .sd-body { flex: 1; overflow-y: auto; padding: 2px 20px 20px; }
.sd-mask .sd-group {
  margin: 16px 0 9px; padding-bottom: 6px; border-bottom: 1px solid #eef3f2;
  font-size: 11px; font-weight: 600; color: #8fa5a2; letter-spacing: .6px;
}

/* 主题卡片：纯色细边框，无毛玻璃 */
.sd-mask .sd-themes { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
.sd-mask .sd-theme {
  display: flex; flex-direction: column; align-items: flex-start; gap: 3px;
  padding: 11px 11px 10px; border: 1px solid #dce7e5; border-radius: 9px;
  background: #fbfdfd; color: #40514f; text-align: left; cursor: pointer;
  transition: border-color .14s ease, background-color .14s ease;
}
.sd-mask .sd-theme:hover:not(:disabled) { border-color: #9dcac5; background: #f4faf8; }
.sd-mask .sd-theme.sd-theme-on { border-color: #0f5f5c; background: #f0f8f6; box-shadow: inset 0 0 0 1px #0f5f5c; }
.sd-mask .sd-theme-top { display: flex; align-items: center; justify-content: space-between; width: 100%; margin-bottom: 4px; }
.sd-mask .sd-theme-icon {
  display: inline-flex; align-items: center; justify-content: center;
  width: 28px; height: 28px; border-radius: 7px;
  background: #eaf3f1; color: #4c7c78;
}
.sd-mask .sd-theme.sd-theme-on .sd-theme-icon { background: #0f5f5c; color: #fff; }
.sd-mask .sd-theme-check { color: #0f5f5c; }
.sd-mask .sd-theme b { font-size: 12.5px; font-weight: 600; color: #26413f; }
.sd-mask .sd-theme.sd-theme-on b { color: #0f5f5c; }
.sd-mask .sd-theme small { font-size: 10.5px; color: #91a3a1; }
.sd-mask .sd-sw { display: flex; gap: 4px; margin-top: 8px; }
.sd-mask .sd-sw i { width: 20px; height: 6px; border-radius: 3px; border: 1px solid rgba(15, 45, 42, .08); }
.sd-mask .sd-theme[data-k="light"] .sd-sw i:nth-child(1) { background: #f7fbfa; border-color: #dce7e5; }
.sd-mask .sd-theme[data-k="light"] .sd-sw i:nth-child(2) { background: #0f5f5c; }
.sd-mask .sd-theme[data-k="light"] .sd-sw i:nth-child(3) { background: #c7e3df; }
.sd-mask .sd-theme[data-k="dark"] .sd-sw i:nth-child(1) { background: #162423; }
.sd-mask .sd-theme[data-k="dark"] .sd-sw i:nth-child(2) { background: #6fd0c4; }
.sd-mask .sd-theme[data-k="dark"] .sd-sw i:nth-child(3) { background: #2c403e; }
.sd-mask .sd-theme[data-k="system"] .sd-sw i:nth-child(1) { background: linear-gradient(90deg, #f7fbfa 50%, #162423 50%); }
.sd-mask .sd-theme[data-k="system"] .sd-sw i:nth-child(2) { background: linear-gradient(90deg, #0f5f5c 50%, #23857d 50%); }
.sd-mask .sd-theme[data-k="system"] .sd-sw i:nth-child(3) { background: linear-gradient(90deg, #c7e3df 50%, #2c403e 50%); }
.sd-mask .sd-theme-note { margin-top: 9px; }
.sd-mask .sd-lang-note { margin-top: 2px; }

.sd-mask .sd-field {
  display: flex; flex-direction: row; align-items: center; justify-content: space-between;
  gap: 12px; padding: 7px 0; font-size: 13px; color: #40514f;
}
.sd-mask .sd-field select { width: 190px; padding: 5px 8px; font-size: 12px; }
.sd-mask .sd-field input[type="checkbox"] { width: 16px; height: 16px; accent-color: #0f5f5c; }
.sd-mask .sd-btns { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin-top: 14px; }
.sd-mask .sd-btn {
  display: inline-flex; align-items: center; gap: 6px;
  padding: 6px 13px; border: 1px solid #0f5f5c; border-radius: 6px;
  background: #0f5f5c; color: #fff; font-size: 12px; cursor: pointer;
}
.sd-mask .sd-btn:hover:not(:disabled) { background: #147570; border-color: #147570; }
.sd-mask .sd-btn.sd-btn-ghost { background: #fff; color: #0f5f5c; }
.sd-mask .sd-btn.sd-btn-ghost:hover:not(:disabled) { background: #e9f4f2; border-color: #0f5f5c; }
.sd-mask .sd-ok { display: inline-flex; align-items: center; gap: 4px; font-size: 12px; color: #0f7a54; }
.sd-mask .sd-note { margin: 14px 0 0; font-size: 11.5px; line-height: 1.7; color: #91a3a1; }
.sd-mask .sd-update { color: #0f5f5c; background: #eef7f5; border-left: 3px solid #0f5f5c; padding: 8px 10px; border-radius: 4px; }
/* 检查更新：按钮置于版本号后方（同一行右端） */
.sd-mask .sd-kv-update { align-items: center; }
.sd-mask .sd-update-tail { display: flex; align-items: center; gap: 12px; }
.sd-mask .sd-btn.sd-btn-sm { padding: 4px 10px; font-size: 11.5px; }
.sd-mask .sd-btn:disabled { opacity: .6; cursor: not-allowed; }
.sd-mask .sd-spin { animation: sd-spin 1s linear infinite; }
@keyframes sd-spin { to { transform: rotate(360deg); } }
.sd-mask .sd-text { margin: 8px 0; font-size: 12.5px; line-height: 1.8; color: #40514f; }
.sd-mask .sd-hero { display: flex; align-items: center; justify-content: center; padding: 22px 0 6px; color: #e08b7d; }
.sd-mask .sd-kv {
  display: flex; align-items: center; justify-content: space-between; gap: 12px;
  padding: 7px 0; border-bottom: 1px solid #f2f6f5; font-size: 12.5px;
}
.sd-mask .sd-kv > span { color: #718684; flex: 0 0 auto; }
.sd-mask .sd-kv > b { font-weight: 500; color: #1c2626; text-align: right; }
.sd-mask .sd-code {
  max-width: 260px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-size: 11px; font-family: Consolas, monospace;
}
.sd-mask .sd-about-head { display: flex; align-items: center; gap: 12px; padding: 18px 0 14px; }
.sd-mask .sd-about-logo {
  display: flex; align-items: center; justify-content: center;
  width: 38px; height: 38px; border-radius: 10px;
  background: #0f5f5c; color: #fff; font-size: 19px; font-weight: 700;
}
.sd-mask .sd-maint-hint { margin: 0 0 2px; }
.sd-mask .sd-maint-msg { font-size: 12px; color: #0f7a54; }
.sd-mask .sd-maint-msg.err { color: #b4462f; }
.sd-mask .sd-about-head b { display: block; font-size: 15px; color: #0f5f5c; }
.sd-mask .sd-about-head small { display: block; margin-top: 2px; font-size: 11.5px; color: #91a3a1; }

/* 意见反馈表单：统一「标签在上、控件铺满整行」的纵向结构，避免横纵混排显得凌乱 */
.sd-mask .sd-form-intro { margin: 2px 0 10px; }
.sd-mask .sd-form { display: flex; flex-direction: column; gap: 13px; }
.sd-mask .sd-frow {
  display: flex; flex-direction: column; align-items: stretch; gap: 6px;
  font-size: 12.5px; color: #40514f;
}
.sd-mask .sd-frow-col { gap: 6px; }
.sd-mask .sd-flabel { color: #718684; font-size: 12.5px; }
.sd-mask .sd-flabel i { margin-left: 3px; color: #c0392b; font-style: normal; }
.sd-mask .sd-flabel em { margin-left: 6px; color: #a0b3b0; font-size: 11px; font-style: normal; }
.sd-mask .sd-frow input[type="text"],
.sd-mask .sd-frow select,
.sd-mask .sd-frow textarea {
  box-sizing: border-box; width: 100%; padding: 7px 9px;
  border: 1px solid #d5e5e2; border-radius: 6px;
  background: #fff; color: #40514f; font-family: inherit; font-size: 12.5px;
}
.sd-mask .sd-frow textarea { min-height: 96px; resize: vertical; line-height: 1.6; }
.sd-mask .sd-fcount { align-self: flex-end; font-size: 10.5px; color: #a8bdb9; }
/* 附件：方形缩略图 + 末尾一个带 + 号的方形添加按钮；未选文件时只剩添加按钮 */
.sd-mask .sd-file-hidden { position: absolute; width: 1px; height: 1px; opacity: 0; pointer-events: none; }
.sd-mask .sd-shots { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 2px; }
.sd-mask .sd-shot {
  position: relative; width: 58px; height: 58px; padding: 0;
  border: 1px solid #dce7e5; border-radius: 7px; background: #fbfdfd; overflow: visible;
}
.sd-mask .sd-shot img { display: block; width: 100%; height: 100%; object-fit: cover; border-radius: 6px; }
.sd-mask .sd-shot-add {
  display: flex; align-items: center; justify-content: center;
  border: 1.5px dashed #a8c8c4; background: #f4faf8; color: #6f9b97; cursor: pointer;
  transition: border-color .14s ease, background-color .14s ease, color .14s ease;
}
.sd-mask .sd-shot-add:hover { border-color: #0f5f5c; background: #e8f4f2; color: #0f5f5c; }
.sd-mask .sd-shot-x {
  position: absolute; top: -7px; right: -7px;
  width: 18px; height: 18px; padding: 0; border: 1px solid #d5e5e2; border-radius: 50%;
  background: #fff; color: #718684; font-size: 13px; line-height: 15px; cursor: pointer;
}
.sd-mask .sd-shot-x:hover { background: #f6e6e1; color: #b4462f; }
.sd-mask .sd-ferr { margin: 0; font-size: 11.5px; color: #b4462f; }

/* 关于页：作者与链接 */
.sd-mask .sd-about-author-title { margin-top: 18px; }
.sd-mask .sd-link {
  color: #0f5f5c; text-decoration: none; font-weight: 500;
  max-width: 260px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.sd-mask .sd-link:hover { text-decoration: underline; }
.sd-mask .sd-kv-author { align-items: flex-start; }
.sd-mask .sd-author-wx { display: flex; flex-direction: row; align-items: center; gap: 6px; }
.sd-mask .sd-author-wx-name { font-size: 12.5px; color: #40514f; }
.sd-mask .sd-wx-qr-btn {
  display: inline-flex; align-items: center; justify-content: center;
  width: 22px; height: 22px; padding: 0; border: 1px solid #d5e5e2; border-radius: 5px;
  background: #f0f8f6; color: #0f5f5c; cursor: pointer;
}
.sd-mask .sd-wx-qr-btn:hover { background: #e2f1ee; }
/* 公众号二维码弹窗 */
.sd-mask .sd-wx-qr-mask {
  position: fixed; inset: 0; z-index: 20;
  display: flex; align-items: center; justify-content: center;
  background: rgba(20, 40, 38, 0.35);
}
.sd-mask .sd-wx-qr-dialog {
  width: 240px; padding: 14px; box-sizing: border-box;
  background: #fff; border-radius: 12px; box-shadow: 0 12px 40px rgba(15, 95, 92, 0.22);
}
.sd-mask .sd-wx-qr-head {
  display: flex; align-items: center; justify-content: space-between;
  margin-bottom: 10px; font-size: 13px; color: #2f4744;
}
.sd-mask .sd-wx-qr-head .sd-x {
  width: 22px; height: 22px; border: none; background: transparent; color: #718684; cursor: pointer;
}
.sd-mask .sd-wx-qr-body { display: flex; flex-direction: column; align-items: center; gap: 10px; }
.sd-mask .sd-wx-qr-ph-mark { transform: rotate(-10deg); font-size: 13px; color: #6f9b97; text-align: center; }

/* 检查更新结果弹窗：复用二维码弹窗外壳，内部换成结果展示 */
.sd-mask .sd-update-modal-body { display: flex; flex-direction: column; align-items: center; gap: 10px; padding: 4px 2px 2px; }
.sd-mask .sd-update-modal-badge {
  display: flex; align-items: center; justify-content: center;
  width: 46px; height: 46px; border-radius: 50%;
  background: #eaf3f1; color: #0f5f5c;
}
.sd-mask .sd-update-modal-msg { margin: 0; max-width: 260px; font-size: 12.5px; line-height: 1.7; color: #40514f; text-align: center; }
.sd-mask .sd-update-notes {
  margin: 0; max-width: 260px; max-height: 160px; overflow: auto;
  padding: 8px 10px; border: 1px solid #dce7e5; border-radius: 7px;
  background: #f6faf9; color: #5c736f; font-size: 11.5px; line-height: 1.6; white-space: pre-wrap; word-break: break-word;
}
.sd-mask .sd-update-download { margin-top: 2px; }
/* 捐赠二维码放大弹窗：弹窗加宽，海报按原始比例自动缩放填充（不裁切、不变形） */
.sd-mask .sd-qr-big-dialog { width: 400px; max-width: calc(100vw - 40px); }
.sd-mask .sd-qr-big-box {
  position: relative; width: 100%; overflow: hidden;
  display: flex; align-items: center; justify-content: center;
  padding: 10px; box-sizing: border-box;
  border: 1px dashed #a8c8c4; border-radius: 10px; background: #fff;
}
/* 真码：保持原始宽高比，在可用空间内自动放大到最大 */
.sd-mask .sd-qr-big-box .dn-qr-real {
  display: block; width: auto; height: auto;
  max-width: 100%; max-height: 62vh; object-fit: contain; background: #fff;
}
/* 占位：正方形网格，与真码区域等宽 */
.sd-mask .sd-qr-big-box .dn-qr-ph { width: 100%; height: auto; aspect-ratio: 1 / 1; opacity: .8; }
.sd-mask .sd-qr-big-box .dn-qr-badge { font-size: 12px; }

/* 公众号二维码弹窗：二维码放大到 260px，下方带引导文案 */
.sd-mask .sd-qr-mp-dialog { width: 320px; max-width: calc(100vw - 40px); }
.sd-mask .sd-qr-mp-box {
  position: relative; width: 260px; height: 260px; overflow: hidden;
  border: 1px solid #d5e5e2; border-radius: 10px; background: #fff;
}
.sd-mask .sd-qr-mp-box .dn-qr-real { display: block; width: 100%; height: 100%; object-fit: contain; background: #fff; }
.sd-mask .sd-qr-mp-ph {
  width: 260px; height: 260px; display: flex; align-items: center; justify-content: center;
  border: 1.5px dashed #b6d3cf; border-radius: 10px; background: #f4faf8;
}
.sd-mask .sd-mp-tip { margin: 0; font-size: 12.5px; color: #40514f; text-align: center; line-height: 1.6; }
/* 捐赠页小二维码上的放大角标 */
.sd-mask .dn-qr-box { padding: 0; border-width: 1.5px; cursor: zoom-in; transition: border-color .14s ease; }
.sd-mask .dn-qr-box:hover { border-color: #0f5f5c; }
.sd-mask .dn-qr-zoom {
  position: absolute; right: 5px; bottom: 5px;
  display: flex; align-items: center; justify-content: center;
  width: 24px; height: 24px; border-radius: 6px;
  background: rgba(15, 95, 92, .82); color: #fff;
}

/* 捐赠：两个二维码占位 + 记录表 */
.sd-mask .dn-title { display: flex; align-items: center; gap: 6px; }
.sd-mask .dn-intro { margin: 4px 0 14px; }
.sd-mask .dn-qrs { display: flex; flex-wrap: wrap; gap: 18px; }
.sd-mask .dn-qr { flex: 0 0 150px; text-align: center; }
.sd-mask .dn-qr-box {
  position: relative; width: 150px; height: 150px; overflow: hidden;
  border: 1px dashed #a8c8c4; border-radius: 10px; background: #f4f8f7;
}
.sd-mask .dn-qr-ph { display: block; width: 100%; height: 100%; opacity: .8; }
.sd-mask .dn-qr-real { display: block; width: 100%; height: 100%; object-fit: contain; background: #fff; }
.sd-mask .dn-qr-badge {
  position: absolute; left: 50%; top: 50%;
  transform: translate(-50%, -50%) rotate(-12deg);
  padding: 5px 10px; border-radius: 5px; background: rgba(15, 95, 92, .9); color: #fff;
  font-size: 11px; letter-spacing: .6px; white-space: nowrap;
}
.sd-mask .dn-qr > b { display: block; margin-top: 8px; font-size: 12.5px; font-weight: 600; color: #40514f; }
.sd-mask .dn-verify { margin-top: 14px; }
.sd-mask .dn-sample { margin-left: 8px; font-size: 10px; font-weight: 500; letter-spacing: 0; color: #b4462f; }
.sd-mask .dn-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.sd-mask .dn-table th,
.sd-mask .dn-table td { padding: 7px 8px; text-align: left; border-bottom: 1px solid #eef3f2; }
.sd-mask .dn-table th { font-size: 11px; font-weight: 600; color: #8fa5a2; }
.sd-mask .dn-table td { color: #40514f; }
.sd-mask .dn-table td:nth-child(3) { color: #0f5f5c; font-weight: 600; }
.sd-mask .dn-table td.dn-empty { text-align: center; color: #91a3a1; padding: 14px 8px; }

:root[data-theme="dark"] .sd-mask { background: rgba(0, 0, 0, .6); }
:root[data-theme="dark"] .sd-mask .sd-dialog { background: #1b2a29; border-color: #33504c; box-shadow: 0 24px 64px rgba(0, 0, 0, .55); color: #dce8e6; }
:root[data-theme="dark"] .sd-mask .sd-head { background: #162423; border-bottom-color: #2c403e; }
:root[data-theme="dark"] .sd-mask .sd-head-icon { background: #23857d; }
:root[data-theme="dark"] .sd-mask .sd-head b { color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-x:hover:not(:disabled) { background: #203230; color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-side { background: #182826; border-right-color: #2c403e; }
:root[data-theme="dark"] .sd-mask .sd-item { color: #c4d8d5; }
:root[data-theme="dark"] .sd-mask .sd-item:hover:not(:disabled) { background: #203230; color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-item.sd-active { background: #23857d; color: #fff; }
:root[data-theme="dark"] .sd-mask .sd-item.sd-action { color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-sep { background: #2c403e; }
:root[data-theme="dark"] .sd-mask .sd-help-note { background: #1d3431; color: #b9cfcc; }
:root[data-theme="dark"] .sd-mask .sd-page-head { border-bottom-color: #2c403e; }
:root[data-theme="dark"] .sd-mask .sd-page-head h3 { color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-group,
:root[data-theme="dark"] .sd-mask .sd-kv { border-color: #2c403e; }
:root[data-theme="dark"] .sd-mask .sd-theme { background: #162423; border-color: #33504c; color: #c4d8d5; }
:root[data-theme="dark"] .sd-mask .sd-theme:hover:not(:disabled) { border-color: #4a7872; background: #1d302e; }
:root[data-theme="dark"] .sd-mask .sd-theme.sd-theme-on { border-color: #6fd0c4; background: #1d3431; box-shadow: inset 0 0 0 1px #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-theme-icon { background: #263b39; color: #9dc9c2; }
:root[data-theme="dark"] .sd-mask .sd-theme.sd-theme-on .sd-theme-icon { background: #23857d; color: #fff; }
:root[data-theme="dark"] .sd-mask .sd-theme-check { color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-theme b { color: #dce8e6; }
:root[data-theme="dark"] .sd-mask .sd-theme.sd-theme-on b { color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-field,
:root[data-theme="dark"] .sd-mask .sd-text,
:root[data-theme="dark"] .sd-mask .sd-kv > b { color: #dce8e6; }
:root[data-theme="dark"] .sd-mask .sd-field select { background: #162423; color: #dce8e6; border-color: #37524f; }
:root[data-theme="dark"] .sd-mask .sd-btn.sd-btn-ghost { background: #162423; color: #6fd0c4; border-color: #37524f; }
:root[data-theme="dark"] .sd-mask .sd-btn.sd-btn-ghost:hover:not(:disabled) { background: #203230; }
:root[data-theme="dark"] .sd-mask .sd-update { background: #1d3431; color: #8fded2; }
:root[data-theme="dark"] .sd-mask .sd-maint-msg { color: #7fd6a8; }
:root[data-theme="dark"] .sd-mask .sd-maint-msg.err { color: #f0a08c; }
:root[data-theme="dark"] .sd-mask .sd-about-head b { color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-code { background: #263b39; }
:root[data-theme="dark"] .sd-mask .sd-frow,
:root[data-theme="dark"] .sd-mask .dn-qr > b,
:root[data-theme="dark"] .sd-mask .dn-table td { color: #dce8e6; }
:root[data-theme="dark"] .sd-mask .sd-flabel { color: #b9cfcc; }
:root[data-theme="dark"] .sd-mask .sd-frow input[type="text"],
:root[data-theme="dark"] .sd-mask .sd-frow select,
:root[data-theme="dark"] .sd-mask .sd-frow textarea { background: #162423; color: #dce8e6; border-color: #37524f; }
:root[data-theme="dark"] .sd-mask .sd-shot-add { background: #162423; border-color: #37524f; color: #7fb3ad; }
:root[data-theme="dark"] .sd-mask .sd-shot-add:hover { border-color: #6fd0c4; background: #1d3431; color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .dn-qr-ph { opacity: .5; }
:root[data-theme="dark"] .sd-mask .sd-shot { background: #162423; border-color: #33504c; }
:root[data-theme="dark"] .sd-mask .sd-shot-x { background: #263b39; border-color: #37524f; color: #c4d8d5; }
:root[data-theme="dark"] .sd-mask .sd-ferr { color: #f0a08c; }
:root[data-theme="dark"] .sd-mask .dn-qr-box { background: #162423; border-color: #37524f; }
:root[data-theme="dark"] .sd-mask .dn-sample { color: #f0a08c; }
:root[data-theme="dark"] .sd-mask .dn-table th,
:root[data-theme="dark"] .sd-mask .dn-table td { border-color: #2c403e; }
:root[data-theme="dark"] .sd-mask .sd-link { color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-author-wx-name { color: #dce8e6; }
:root[data-theme="dark"] .sd-mask .sd-wx-qr-dialog { background: #1f2e2c; box-shadow: 0 12px 40px rgba(0, 0, 0, 0.4); }
:root[data-theme="dark"] .sd-mask .sd-wx-qr-head { color: #dce8e6; }
:root[data-theme="dark"] .sd-mask .sd-wx-qr-ph-mark { color: #7fb3ad; }
:root[data-theme="dark"] .sd-mask .sd-wx-qr-btn { background: #142523; border-color: #37524f; color: #7fd6a8; }
/* 暗色：检查更新按钮 / 结果弹窗 / 二维码放大 */
:root[data-theme="dark"] .sd-mask .sd-update-modal-badge { background: #263b39; color: #6fd0c4; }
:root[data-theme="dark"] .sd-mask .sd-update-modal-msg { color: #dce8e6; }
:root[data-theme="dark"] .sd-mask .sd-update-notes { background: #162423; border-color: #37524f; color: #a9c0bc; }
:root[data-theme="dark"] .sd-mask .dn-qr-real { background: #fff; }
:root[data-theme="dark"] .sd-mask .sd-qr-big-box { background: #162423; border-color: #37524f; }
:root[data-theme="dark"] .sd-mask .sd-qr-mp-box { background: #162423; border-color: #37524f; }
:root[data-theme="dark"] .sd-mask .sd-qr-mp-ph { background: #162423; border-color: #37524f; }
:root[data-theme="dark"] .sd-mask .sd-mp-tip { color: #c4d8d5; }
:root[data-theme="dark"] .sd-mask .dn-qr-box:hover { border-color: #6fd0c4; }
</style>
