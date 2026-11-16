<script setup lang="ts">
/**
 * Grename M1：产品化交互（选择对象 → 设规则 → 实时预览 → 计划 → 执行 → 撤销）
 * 设计文档 v1.3 §3 / §6.2 / §6.6 / §12.1。
 *
 * 关键约束：预览由后端 preview_rule 计算，与 build_plan 共用 Rust 规则引擎，
 * 前端绝不用 JS RegExp 自己算，避免「预览改成 A、实际改成 B」。
 *
 * 本轮改动：
 *  - shell 改为纵向 flex（标题栏固定 / 内容区滚动 / 状态栏固定）；
 *  - 原「流式进度」面板移入底部状态栏并改名「进度」；
 *  - 底部日志行移除，日志位置改到设置页；
 *  - 全界面文案走 i18n，状态名默认中文，可中英文切换；
 *  - 历史任务行接入 export_report（CSV 报告）与 resume_interrupted（崩溃收尾）。
 */
import { computed, onMounted, onUnmounted, reactive, ref, watch } from "vue";
import { Channel, invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { Download, RotateCcw } from "@lucide/vue";
import TitleBar from "./components/TitleBar.vue";
import SplashScreen from "./components/SplashScreen.vue";
import { locale, phaseLabel, statusLabel, t } from "./i18n/index";
import { createFrameProbe, installFrameProbeDebug } from "./frameProbe";

interface Progress {
  jobId: string; phase: string; outputMode: string;
  done: number; total: number; failed: number; skipped: number;
  bytesDone: number; bytesTotal: number;
  speed: number; bytesPerSec: number; etaMs: number; elapsedMs: number;
}
interface JobInfo {
  id: string; status: string; rootPath: string; recursive: boolean;
  outputMode: string; outputDir: string | null;
  total: number; bytesTotal: number; done: number; bytesDone: number;
  failed: number; skipped: number; conflicted: number;
  undoAvailable: boolean; undoPrecheckAt: number | null;
  undoPrecheckToken: string | null; createdAt: number;
}
interface EntryRow {
  id: number; seq: number; srcPath: string; srcName: string; srcSize: number;
  relDir: string; dstPath: string; dstName: string;
  status: string; errCode: string | null; undoCheck: string | null;
}
interface EntryPage { items: EntryRow[]; nextCursor: number | null; total: number }
interface PlanSummary {
  jobId: string; total: number; planned: number; conflicted: number;
  bytesTotal: number; outputDir: string | null;
}
interface ConflictCount { code: string; count: number }
interface PrecheckResult {
  jobId: string; total: number; okCount: number;
  /** 复核时自愈的「文件已还原、DB 未回写」条目数（崩溃恢复，见 M2 扩展报告）。 */
  alreadyUndone?: number;
  conflicts: ConflictCount[]; token: string; expiresAt: number;
}
interface UndoResult { jobId: string; undone: number; conflicts: number; status: string }
interface ApiError { code?: string; message?: string }
/** M3：export_report 返回；M2：resume_interrupted 返回（serde camelCase）。 */
interface ExportResult { path: string; rows: number }
interface RecoverResult {
  jobId: string; partsRemoved: number; reconciledDone: number;
  markedSkipped: number; needsReview: number; undoAvailable: boolean; status: string;
}
interface RuleSpec {
  pattern: string; replacement: string; scope: string; filter: string;
  caseInsensitive: boolean; matchAll: boolean; seqStart: number; seqWidth: number;
}
interface PreviewItem {
  id: number; srcName: string; dstName: string | null;
  status: string; reason: string | null;
}

/**
 * v1.17 默认值后端化（收尾 B）：
 * 设置弹窗里的「默认递归子目录 / 默认输出去向」此前是**死配置**——只写 localStorage，
 * 主界面却硬编码 false / in_place，改了不生效。这里首帧直接读本地快照（零延迟、无闪烁），
 * 之后由设置弹窗保存后广播的事件刷新；后端 SQLite 是权威副本（见 SettingsPopover）。
 */
const PREFS_KEY = "grename.settings";
function readLocalDefaults(): { recursive: boolean; outputMode: "in_place" | "to_dir" } {
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (!raw) return { recursive: false, outputMode: "in_place" };
    const p = JSON.parse(raw) as { recursive?: unknown; outputMode?: unknown };
    return {
      recursive: p.recursive === true,
      outputMode: p.outputMode === "to_dir" ? "to_dir" : "in_place",
    };
  } catch {
    // 本地偏好损坏时回落默认值，绝不因此让界面起不来。
    return { recursive: false, outputMode: "in_place" };
  }
}

const rootPath = ref("");
const selectedFiles = ref<string[]>([]);
const outputDir = ref("");
const initialDefaults = readLocalDefaults();
const recursive = ref(initialDefaults.recursive);
const outputMode = ref<"in_place" | "to_dir">(initialDefaults.outputMode);
const rule = reactive<RuleSpec>({
  pattern: "",
  replacement: "ren_$0",
  scope: "name",
  filter: "",
  caseInsensitive: false,
  matchAll: true,
  seqStart: 1,
  seqWidth: 0,
});

const jobId = ref("");
const token = ref("");
const busy = ref(false);
const lastMsg = ref("");
const lastMsgErr = ref(false);
const currentJob = ref<JobInfo | null>(null);
const plan = ref<PlanSummary | null>(null);
const precheck = ref<PrecheckResult | null>(null);
const undoPending = ref(false);
/** M3 收尾 A：撤销复核冲突明细分页；后端 get_undo_conflicts 已注册，前端只做展示。 */
const conflictOpen = ref(false);
const conflictRows = ref<EntryRow[]>([]);
const conflictTotal = ref(0);
const conflictNextCursor = ref<number | null>(null);
/** 每页起始 cursor 栈，用于「上一页」回溯；首页恒为 0。 */
const conflictCursors = ref<number[]>([0]);
const conflictBusy = ref(false);
const history = ref<JobInfo[]>([]);
const previewError = ref("");
const previewMap = ref<Map<number, PreviewItem>>(new Map());
const plannedRuleSig = ref("");

const progress = ref<Progress | null>(null);
let pending: Progress | null = null;
let rafId = 0;
let scrollTimer = 0;

// 帧率埋点本体保留（start/stop/日志/CSV 不变），仅移除专供 header 三个性能指标的 onFps 显示回调。
const frameProbe = createFrameProbe();
/** 任务窗口结束时把结构化摘要写入 grename.log，供 §12.3 ⑥ GUI 手工回归取数。 */
function logFrameSummary() {
  const s = frameProbe.stop();
  void invoke("append_log", { line: `[frame-probe] ${JSON.stringify(s)}` }).catch(() => {});
  frameProbe.start("idle", 0);
}
let previewTimer = 0;

const rows = ref<EntryRow[]>([]);
const rowTotal = ref(0);
const nextCursor = ref<number | null>(0);
const statusFilter = ref("");
const scrollTop = ref(0);
const ROW_H = 28;
const VIEW_H = 300;
// needs_review 来自 resume_interrupted：源与目标都不存在，列入人工核查，绝不猜测。
const STATUS_CODES = ["scanned", "planned", "skipped", "conflict", "done", "failed", "undone", "needs_review"];
const startIndex = computed(() => Math.max(0, Math.floor(scrollTop.value / ROW_H) - 3));
const visibleRows = computed(() => {
  const start = startIndex.value;
  const count = Math.ceil(VIEW_H / ROW_H) + 6;
  return rows.value.slice(start, start + count).map((r, i) => ({ r, i: start + i }));
});

const hasSelection = computed(() => !!rootPath.value.trim() || selectedFiles.value.length > 0);
const ruleSig = computed(() => JSON.stringify({ ...rule }));
const ruleDirty = computed(() => !!plan.value && ruleSig.value !== plannedRuleSig.value);
const nextStep = computed(() => {
  if (busy.value) return t("guide.busy");
  if (!hasSelection.value) return t("guide.select");
  if (!jobId.value) return t("guide.scan");
  const s = currentJob.value?.status;
  if (s === "completed" || s === "undone" || s === "cancelled")
    return t("guide.ended");
  if (previewError.value) return t("guide.ruleInvalid");
  if (!plan.value || ruleDirty.value) return t("guide.plan");
  return t("guide.execute");
});
const phaseText = computed(() => phaseLabel(progress.value?.phase ?? ""));
/** header 运行状态徽章：就绪 / 运行中（phase）/ 任务终态（status），复用现有 phase/status 文案。 */
const headerState = computed(() => {
  if (busy.value) {
    const ph = progress.value?.phase ?? "";
    return { text: ph ? phaseLabel(ph) : t("hdr.working"), tone: "run" };
  }
  const s = currentJob.value?.status ?? "";
  if (!s) return { text: t("hdr.ready"), tone: "idle" };
  const tone = s === "completed" || s === "done" || s === "undone" ? "done"
    : s === "interrupted" || s === "failed" || s === "cancelled" ? "warn" : "idle";
  return { text: statusLabel(s), tone };
});
const precheckHtml = computed(() => {
  const p = precheck.value;
  if (!p) return "";
  const codes = p.conflicts.map(c => `${statusLabel(c.code)}×${c.count}`).join("、") || t("common.none");
  return t("notice.precheck", { ok: p.okCount, total: p.total, conflict: p.total - p.okCount, codes });
});
const planNotice = computed(() => {
  const p = plan.value;
  return p ? t("notice.plan", { planned: p.planned, conflicted: p.conflicted, bytes: fmtBytes(p.bytesTotal) }) : "";
});

/** 流水日志只追加到 grename.log；状态栏仅保留最近一条状态，避免占版面。 */
function say(msg: string) {
  const ts = new Date().toLocaleString(locale.value, { hour12: false });
  lastMsg.value = msg;
  lastMsgErr.value = /失败|拒绝|错误|请先|不能|无效|不存在|超时|failed|rejected|error|invalid|missing|please/i.test(msg);
  void invoke("append_log", { line: `[${ts}] ${msg}` }).catch(() => {
    // 日志写入失败不影响主流程，也不再往界面回显
  });
}
function errText(e: unknown): string {
  const x = e as ApiError;
  return x?.code ? `${x.code}: ${x.message ?? ""}` : String(e);
}
function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const u = ["KB", "MB", "GB", "TB"];
  let i = -1, v = n;
  do { v /= 1024; i++; } while (v >= 1024 && i < u.length - 1);
  return `${v.toFixed(2)} ${u[i]}`;
}
function fmtEta(ms: number): string {
  if (!ms) return "—";
  const s = Math.round(ms / 1000);
  return s >= 60 ? `${Math.floor(s / 60)}m${s % 60}s` : `${s}s`;
}

/** rAF 合并：进度事件 ≤12.5/s，渲染不逐事件触发组件更新。 */
function onProgress(p: Progress) {
  pending = p;
  if (!rafId) {
    rafId = requestAnimationFrame(() => {
      rafId = 0;
      if (pending) progress.value = pending;
    });
  }
}
function newChannel(): Channel<Progress> {
  const ch = new Channel<Progress>();
  ch.onmessage = onProgress;
  return ch;
}
/** 捕获阶段监听所有可滚动容器；滚动时显示滚动条，停 900ms 后淡出。 */
function onAnyScroll() {
  const root = document.documentElement;
  root.classList.add("scrolling");
  window.clearTimeout(scrollTimer);
  scrollTimer = window.setTimeout(() => root.classList.remove("scrolling"), 900);
}
/**
 * 设置弹窗保存后重新套用默认值。
 * 只在「还没开始任何任务、也没手动选文件」时生效：一旦进入扫描 / 执行或已载入历史任务，
 * 界面上的递归与去向反映的是**当前任务**的取值（见 selectHistory），不能被默认值冲掉。
 */
function applyDefaultPrefs() {
  if (jobId.value || selectedFiles.value.length > 0) return;
  const d = readLocalDefaults();
  recursive.value = d.recursive;
  outputMode.value = d.outputMode;
}
/**
 * 启动页：只做一层视觉遮罩，不阻塞任何业务初始化（上面的刷新照常跑）。
 * 展示 1.6s（进度条 1.15s 填满后留约 0.25s 余量），再由 <Transition> 淡出 0.45s。
 * 定时器在 onUnmounted 清理，避免热更新 / 窗口销毁后残留回调。
 */
const showSplash = ref(true);
let splashTimer = 0;
onMounted(() => {
  frameProbe.start("idle", 0);
  installFrameProbeDebug(frameProbe);
  window.addEventListener("scroll", onAnyScroll, true);
  window.addEventListener("grename:prefs-saved", applyDefaultPrefs);
  refreshHistory();
  void refreshTemplates();
  say(t("msg.started"));
  splashTimer = window.setTimeout(() => { showSplash.value = false; }, 1600);
});
onUnmounted(() => {
  window.removeEventListener("scroll", onAnyScroll, true);
  window.removeEventListener("grename:prefs-saved", applyDefaultPrefs);
  window.clearTimeout(scrollTimer);
  window.clearTimeout(previewTimer);
  window.clearTimeout(splashTimer);
  frameProbe.stop();
  cancelAnimationFrame(rafId);
});

const pctFiles = computed(() => {
  const p = progress.value;
  return p && p.total > 0 ? Math.min(100, (p.done / p.total) * 100) : 0;
});
const pctBytes = computed(() => {
  const p = progress.value;
  return p && p.bytesTotal > 0 ? Math.min(100, (p.bytesDone / p.bytesTotal) * 100) : 0;
});

async function refreshJob() {
  if (!jobId.value) return;
  try { currentJob.value = await invoke<JobInfo>("get_job", { jobId: jobId.value }); }
  catch (e) { say(t("msg.readJobFailed", { err: errText(e) })); }
}
/** 带超时：后台线程异常导致状态卡住时，不允许界面永久锁死。 */
async function waitJob(nonTerminal: string, timeoutMs = 30 * 60 * 1000) {
  return new Promise<void>((resolve) => {
    const started = Date.now();
    const timer = window.setInterval(async () => {
      if (Date.now() - started > timeoutMs) {
        window.clearInterval(timer);
        say(t("msg.waitTimeout"));
        resolve();
        return;
      }
      try {
        const j = await invoke<JobInfo>("get_job", { jobId: jobId.value });
        currentJob.value = j;
        if (j.status !== nonTerminal) { window.clearInterval(timer); resolve(); }
      } catch { window.clearInterval(timer); resolve(); }
    }, 300);
  });
}

// --- 原生选择 -------------------------------------------------------------
async function doPickDir() {
  try {
    const r = await open({ directory: true, multiple: false, title: t("dlg.dir") });
    if (typeof r === "string" && r) {
      rootPath.value = r; selectedFiles.value = [];
      say(t("msg.dirPicked", { path: r }));
    }
  } catch (e) { say(t("msg.pickDirFailed", { err: errText(e) })); }
}
async function doPickFiles() {
  try {
    const r = await open({ directory: false, multiple: true, title: t("dlg.files") });
    if (Array.isArray(r) && r.length) {
      selectedFiles.value = r; rootPath.value = "";
      say(t("msg.filesPicked", { n: r.length }));
    }
  } catch (e) { say(t("msg.pickFilesFailed", { err: errText(e) })); }
}
async function doPickOutputDir() {
  try {
    const r = await open({ directory: true, multiple: false, title: t("dlg.out") });
    if (typeof r === "string" && r) { outputDir.value = r; say(t("msg.outputDirPicked", { path: r })); }
  } catch (e) { say(t("msg.pickOutputDirFailed", { err: errText(e) })); }
}
/** 规则预设：每次都先复位到 BASE，避免上一个预设的字段残留。 */
interface Preset { key: string; rule: Partial<RuleSpec> }
const PRESET_BASE: RuleSpec = {
  pattern: "", replacement: "", scope: "name", filter: "",
  caseInsensitive: false, matchAll: true, seqStart: 1, seqWidth: 0,
};
const PRESETS: Preset[] = [
  { key: "prefix", rule: { replacement: "ren_$0" } },
  { key: "suffix", rule: { replacement: "$0_已处理" } },
  { key: "seq", rule: { replacement: "file_{n}", seqWidth: 4 } },
  { key: "seqname", rule: { scope: "full", replacement: "{n}_{name}", seqWidth: 3 } },
  { key: "ext", rule: { scope: "ext", replacement: "jpg" } },
  { key: "imgnum", rule: { filter: "\\.(jpg|jpeg|png|webp)$", replacement: "IMG_{n}", seqWidth: 4, caseInsensitive: true } },
  { key: "space", rule: { scope: "full", pattern: "\\s+", replacement: "_" } },
  { key: "dedup", rule: { pattern: "\\s*\\(\\d+\\)", replacement: "" } },
  { key: "date", rule: { scope: "full", pattern: "(\\d{4})[-_.](\\d{1,2})[-_.](\\d{1,2})", replacement: "$1-$2-$3" } },
  { key: "clean", rule: { scope: "full", pattern: "[^\\w.\\-]+", replacement: "_" } },
  { key: "clear", rule: {} },
];
function applyPreset(p: Preset) {
  Object.assign(rule, PRESET_BASE, p.rule);
  say(t("msg.presetApplied", { name: t(`preset.${p.key}`) }));
}

/** v1.15 自定义规则模板：后端 SQLite 持久化，模板包不含绝对路径。 */
interface TemplateInfo {
  id: string; name: string; ruleJson: string; createdAt: number; updatedAt: number;
}
interface ImportResult { imported: number; skipped: number; errors: string[] }
const templates = ref<TemplateInfo[]>([]);
const selectedTemplateId = ref("");
const paused = ref(false);

async function refreshTemplates() {
  try { templates.value = await invoke<TemplateInfo[]>("list_templates"); }
  catch (e) { say(t("tpl.loadFailed", { err: errText(e) })); }
}
/** 只序列化规则与输出模式偏好，绝不写入 outputDir / rootPath。 */
function currentRuleJson(): string {
  return JSON.stringify({ ...rule, outputMode: outputMode.value });
}
async function saveCurrentTemplate() {
  const name = window.prompt(t("tpl.namePrompt"));
  if (!name) return;
  try {
    await invoke("save_template", { name, ruleJson: currentRuleJson() });
    await refreshTemplates();
    say(t("tpl.saved", { name }));
  } catch (e) { say(t("tpl.failed", { err: errText(e) })); }
}
async function applySelectedTemplate() {
  const item = templates.value.find((x) => x.id === selectedTemplateId.value);
  if (!item) { say(t("tpl.needSelect")); return; }
  try {
    const obj = JSON.parse(item.ruleJson) as Partial<RuleSpec> & { outputMode?: string };
    const { outputMode: om, ...rest } = obj;
    Object.assign(rule, PRESET_BASE, rest);
    if (om === "in_place" || om === "to_dir") outputMode.value = om;
    say(t("tpl.applied", { name: item.name }));
  } catch (e) { say(t("tpl.failed", { err: errText(e) })); }
}
async function deleteSelectedTemplate() {
  if (!selectedTemplateId.value) { say(t("tpl.needSelect")); return; }
  try {
    await invoke("delete_template", { id: selectedTemplateId.value });
    selectedTemplateId.value = "";
    await refreshTemplates();
    say(t("tpl.deleted"));
  } catch (e) { say(t("tpl.failed", { err: errText(e) })); }
}
async function exportTemplatesToFile() {
  try {
    const path = await save({ filters: [{ name: "JSON", extensions: ["json"] }],
                              defaultPath: "grename-templates.json" });
    if (!path) return;
    const n = await invoke<number>("export_templates", { path, ids: null });
    say(t("tpl.exported", { n }));
  } catch (e) { say(t("tpl.failed", { err: errText(e) })); }
}
async function importTemplatesFromFile() {
  try {
    const path = await open({ filters: [{ name: "JSON", extensions: ["json"] }], multiple: false });
    if (typeof path !== "string" || !path) return;
    const r = await invoke<ImportResult>("import_templates", { path });
    await refreshTemplates();
    say(t("tpl.imported", { n: r.imported, s: r.skipped }) +
        (r.errors.length ? ` · ${r.errors[0]}` : ""));
  } catch (e) { say(t("tpl.failed", { err: errText(e) })); }
}
/** 暂停只置运行时标志；批次边界生效，不改任务状态与 DB。 */
async function togglePause() {
  if (!jobId.value) return;
  const cmd = paused.value ? "resume_job" : "pause_job";
  try {
    const r = await invoke<{ ok: boolean }>(cmd, { jobId: jobId.value });
    if (!r.ok) { say(t("msg.pauseNone")); return; }
    paused.value = !paused.value;
    say(t(paused.value ? "msg.pauseOk" : "msg.resumeOk"));
  } catch (e) { say(t("tpl.failed", { err: errText(e) })); }
}


// --- 实时预览（后端算，前端只展示） ---------------------------------------
async function refreshPreview() {
  if (!jobId.value || rows.value.length === 0) return;
  try {
    const items = await invoke<PreviewItem[]>("preview_rule", {
      jobId: jobId.value, rule: { ...rule }, cursor: 0,
      limit: Math.min(1000, Math.max(200, rows.value.length)),
    });
    const m = new Map<number, PreviewItem>();
    for (const it of items) m.set(it.id, it);
    previewMap.value = m;
    previewError.value = "";
  } catch (e) {
    previewError.value = errText(e);
    previewMap.value = new Map();
  }
}
watch(jobId, () => { if (conflictOpen.value) closeConflicts(); });
watch(ruleSig, () => {
  window.clearTimeout(previewTimer);
  previewTimer = window.setTimeout(() => { void refreshPreview(); }, 220);
});
function rowDst(r: EntryRow): string {
  const p = previewMap.value.get(r.id);
  if (p) {
    if (p.dstName) return p.dstName;
    return p.reason === "FILTERED" ? t("dst.filtered")
      : p.reason === "NO_CHANGE" ? t("dst.noChange") : t("dst.invalid");
  }
  return r.dstName || "—";
}
function rowBadge(r: EntryRow): string {
  const p = previewMap.value.get(r.id);
  if (p) return p.dstName ? "planned" : "skipped";
  return r.undoCheck && r.undoCheck !== "ok" ? r.undoCheck : (r.errCode || r.status);
}

// --- 主流程 ---------------------------------------------------------------
async function doScan() {
  if (!hasSelection.value) { say(t("msg.needSelection")); return; }
  busy.value = true;
  frameProbe.start("scan", 0);
  progress.value = null;
  plan.value = null; precheck.value = null; token.value = "";
  undoPending.value = false; previewMap.value = new Map(); plannedRuleSig.value = "";
  const paths = selectedFiles.value.length ? selectedFiles.value : null;
  const isRecursive = paths ? false : recursive.value;
  say(t("msg.scanStarted", {
    target: paths ? t("select.picked", { n: paths.length }) : rootPath.value,
    recursive: String(isRecursive),
  }));
  try {
    const r = await invoke<{ jobId: string }>("scan_files", {
      rootPath: rootPath.value.trim(),
      recursive: isRecursive,
      paths,
      channel: newChannel(),
    });
    jobId.value = r.jobId;
    say(t("msg.scanning", { id: r.jobId }));
    await waitJob("scanning");
    await refreshJob();
    say(t("msg.scanDone", {
      n: currentJob.value?.total ?? 0,
      size: fmtBytes(currentJob.value?.bytesTotal ?? 0),
    }));
    await loadEntries(true);
    await refreshPreview();
    await refreshHistory();
  } catch (e) { say(t("msg.scanFailed", { err: errText(e) })); }
  finally { logFrameSummary(); busy.value = false; }
}

async function doPlan() {
  if (!jobId.value) { say(t("msg.needScan")); return; }
  const s = currentJob.value?.status;
  if (s === "completed" || s === "undone" || s === "cancelled" || s === "interrupted") {
    say(t("msg.jobReadOnly")); return;
  }
  if (previewError.value) { say(t("msg.ruleInvalid")); return; }
  busy.value = true;
  try {
    plan.value = await invoke<PlanSummary>("build_plan", {
      jobId: jobId.value,
      outputMode: outputMode.value,
      outputDir: outputMode.value === "to_dir" ? outputDir.value.trim() || null : null,
      rule: { ...rule },
    });
    plannedRuleSig.value = ruleSig.value;
    previewMap.value = new Map();
    say(t("msg.planDone", {
      planned: plan.value.planned, conflicted: plan.value.conflicted,
      bytes: fmtBytes(plan.value.bytesTotal),
    }));
    await loadEntries(true);
  } catch (e) { say(t("msg.planFailed", { err: errText(e) })); }
  finally { busy.value = false; }
}

async function doExecute() {
  if (!jobId.value || !plan.value) { say(t("msg.needPlan")); return; }
  const s = currentJob.value?.status;
  if (s === "completed" || s === "undone" || s === "interrupted") {
    say(t("msg.cannotExec")); return;
  }
  if (ruleDirty.value) { say(t("msg.ruleDirty")); return; }
  busy.value = true;
  frameProbe.start("execute", plan.value?.total ?? 0);
  progress.value = null; precheck.value = null; token.value = "";
  say(t("msg.execStarted", {
    mode: t(outputMode.value === "in_place" ? "msg.execStarted.inPlace" : "msg.execStarted.toDir"),
  }));
  try {
    await invoke("start_execute", { jobId: jobId.value, channel: newChannel() });
    await waitJob("executing");
    await refreshJob();
    say(t("msg.execDone", {
      status: currentJob.value?.status ?? "—",
      done: currentJob.value?.done ?? 0,
      failed: currentJob.value?.failed ?? 0,
    }));
    await loadEntries(true);
    await refreshHistory();
  } catch (e) { say(t("msg.execFailed", { err: errText(e) })); }
  finally { logFrameSummary(); busy.value = false; paused.value = false; }
}

async function doCancel() {
  if (!jobId.value) return;
  const r = await invoke<{ ok: boolean }>("cancel_job", { jobId: jobId.value });
  paused.value = false;
  say(r.ok ? t("msg.cancelOk") : t("msg.cancelNone"));
}

/** 一键撤销：内部先全量复核，无冲突直接回滚；有冲突则暂停，等用户确认。 */
async function doUndo() {
  if (!jobId.value) { say(t("msg.needJob")); return; }
  busy.value = true; progress.value = null;
  frameProbe.start("undo", currentJob.value?.total ?? 0);
  undoPending.value = false;
  say(t("msg.undoStart"));
  try {
    const p = await invoke<PrecheckResult>("precheck_undo", {
      jobId: jobId.value, channel: newChannel(),
    });
    precheck.value = p; token.value = p.token;
    const conflicts = p.total - p.okCount;
    if (conflicts > 0) {
      undoPending.value = true;
      const c = p.conflicts.map(x => `${statusLabel(x.code)}×${x.count}`).join(", ") || "—";
      say(t("msg.undoConflicts", { n: conflicts, codes: c, ok: p.okCount }));
      return;
    }
    say(t("msg.undoPrecheckOk", { n: p.okCount }));
    await runUndo();
  } catch (e) { say(t("msg.undoRejected", { err: errText(e) })); token.value = ""; }
  finally { logFrameSummary(); busy.value = false; }
}
async function confirmUndo() {
  busy.value = true;
  frameProbe.start("undo-confirm", currentJob.value?.total ?? 0);
  try { await runUndo(); }
  catch (e) { say(t("msg.undoRejected", { err: errText(e) })); token.value = ""; }
  finally { logFrameSummary(); busy.value = false; }
}
async function runUndo() {
  const r = await invoke<UndoResult>("undo_job", {
    jobId: jobId.value, token: token.value, channel: newChannel(),
  });
  say(t("msg.undoDone", { undone: r.undone, conflicts: r.conflicts, status: r.status }));
  token.value = ""; precheck.value = null; undoPending.value = false;
  await refreshJob(); await loadEntries(true); await refreshHistory();
}

async function loadEntries(reset = false) {
  if (!jobId.value) return;
  if (reset) { rows.value = []; nextCursor.value = 0; scrollTop.value = 0; }
  if (nextCursor.value === null) return;
  try {
    const page = await invoke<EntryPage>("get_entries", {
      jobId: jobId.value,
      cursor: nextCursor.value,
      limit: 200,
      status: statusFilter.value || null,
    });
    rows.value = reset ? page.items : rows.value.concat(page.items);
    rowTotal.value = page.total;
    nextCursor.value = page.nextCursor;
  } catch (e) { say(t("msg.readEntriesFailed", { err: errText(e) })); }
}
function onScroll(e: Event) { scrollTop.value = (e.target as HTMLElement).scrollTop; }

/** 冲突原因码：优先取撤销复核结果，其次错误码 / 任务状态。 */
function conflictCode(r: EntryRow): string {
  return r.undoCheck && r.undoCheck !== "ok" ? r.undoCheck : (r.errCode || r.status);
}

/** 只拉当前页（limit 200），不一次取全量，保持内存恒定。 */
async function loadConflictPage(cursor: number, push: boolean) {
  if (!jobId.value) return;
  conflictBusy.value = true;
  try {
    const page = await invoke<EntryPage>("get_undo_conflicts", {
      jobId: jobId.value, cursor, limit: 200,
    });
    conflictRows.value = page.items;
    conflictTotal.value = page.total;
    conflictNextCursor.value = page.nextCursor;
    if (push) conflictCursors.value = [...conflictCursors.value, cursor];
  } catch (e) {
    say(t("msg.conflictsFailed", { err: errText(e) }));
  } finally {
    conflictBusy.value = false;
  }
}
async function toggleConflicts() {
  if (conflictOpen.value) { closeConflicts(); return; }
  conflictOpen.value = true;
  conflictCursors.value = [0];
  await loadConflictPage(0, false);
}
function closeConflicts() {
  conflictOpen.value = false;
  conflictRows.value = [];
  conflictTotal.value = 0;
  conflictNextCursor.value = null;
  conflictCursors.value = [0];
}
async function conflictNext() {
  if (conflictNextCursor.value === null || conflictBusy.value) return;
  await loadConflictPage(conflictNextCursor.value, true);
}
async function conflictPrev() {
  if (conflictCursors.value.length < 2 || conflictBusy.value) return;
  const stack = conflictCursors.value.slice(0, -1);
  conflictCursors.value = stack;
  await loadConflictPage(stack[stack.length - 1], false);
}

async function refreshHistory() {
  try { history.value = await invoke<JobInfo[]>("list_jobs", { limit: 30 }); }
  catch (e) { say(t("msg.readHistoryFailed", { err: errText(e) })); }
}
async function selectHistory(j: JobInfo) {
  jobId.value = j.id;
  rootPath.value = j.rootPath;
  selectedFiles.value = [];
  recursive.value = j.recursive;
  outputMode.value = j.outputMode === "to_dir" ? "to_dir" : "in_place";
  outputDir.value = j.outputDir ?? "";
  token.value = ""; precheck.value = null; plan.value = null;
  undoPending.value = false; previewMap.value = new Map(); plannedRuleSig.value = "";
  await refreshJob(); await loadEntries(true);
  say(t("msg.historySelected", {
    id: j.id, status: j.status,
    undo: j.undoAvailable ? t("common.yes") : t("common.no"),
  }));
}

/** 导出某任务的逐条结果 CSV；路径一律由系统保存对话框给出，不让用户手输绝对路径。 */
async function exportHistory(j: JobInfo) {
  if (busy.value) return;
  let path: string | null;
  try {
    path = await save({
      title: t("dlg.export"),
      defaultPath: `grename_${j.id.slice(0, 14)}.csv`,
      filters: [{ name: "CSV", extensions: ["csv"] }],
    });
  } catch (e) { say(t("msg.exportFailed", { err: errText(e) })); return; }
  if (!path) return; // 用户取消
  busy.value = true;
  try {
    const r = await invoke<ExportResult>("export_report", { jobId: j.id, path });
    say(t("msg.exportDone", { rows: r.rows, path: r.path }));
  } catch (e) { say(t("msg.exportFailed", { err: errText(e) })); }
  finally { busy.value = false; }
}

/** 仅对 interrupted 任务可用：清理 .part、按文件系统反推条目、收尾后仍可撤销。 */
async function resumeJob(j: JobInfo) {
  if (busy.value) return;
  if (j.status !== "interrupted") { say(t("msg.resumeNotNeeded")); return; }
  busy.value = true;
  try {
    const r = await invoke<RecoverResult>("resume_interrupted", { jobId: j.id });
    say(t("msg.resumeDone", {
      done: r.reconciledDone, skipped: r.markedSkipped,
      review: r.needsReview, parts: r.partsRemoved, status: r.status,
    }));
    await refreshHistory();
    if (j.id === jobId.value) { await refreshJob(); await loadEntries(true); }
  } catch (e) { say(t("msg.resumeFailed", { err: errText(e) })); }
  finally { busy.value = false; }
}
</script>

<template>
  <div class="shell">
    <!-- 启动页：首帧覆盖，1.6s 后由 Transition 淡出；纯视觉层，不影响下方布局与初始化 -->
    <Transition name="splash"><SplashScreen v-if="showSplash" /></Transition>
    <TitleBar />

    <div class="app-scroll">
      <div class="app">
        <header>
          <div>
            <h1>Grename <span class="tag">{{ t("app.tag") }}</span></h1>
            <p class="sub">{{ t("app.sub") }}</p>
          </div>
          <div class="hdr-state" :class="headerState.tone" :title="t('hdr.stateTip')" role="status">
            <i class="hdr-dot" aria-hidden="true"></i><span>{{ headerState.text }}</span>
          </div>
        </header>

        <div class="top-split">
          <section class="panel">
            <h2>{{ t("step.select") }}</h2>
            <div class="row">
              <button class="mini" :disabled="busy" @click="doPickDir">{{ t("select.dir") }}</button>
              <button class="mini" :disabled="busy" @click="doPickFiles">{{ t("select.files") }}</button>
              <label class="check"><input type="checkbox" v-model="recursive" :disabled="selectedFiles.length > 0" /> {{ t("select.recursive") }}</label>
            </div>
            <div class="pickinfo" :title="selectedFiles.join('\n') || rootPath">
              {{ selectedFiles.length ? t("select.picked", { n: selectedFiles.length }) : (rootPath || t("select.none")) }}
            </div>
            <p class="hint">{{ t("select.hint") }}</p>
          </section>

          <section class="panel">
            <h2>{{ t("step.output") }}</h2>
            <div class="grid">
              <label>{{ t("output.mode") }}
                <select v-model="outputMode">
                  <option value="in_place">{{ t("output.mode.inPlace") }}</option>
                  <option value="to_dir">{{ t("output.mode.toDir") }}</option>
                </select>
              </label>
              <!-- 常驻不隐藏：隐藏会让右侧卡片高度跳动；非 to_dir 时整体置灰且不可操作 -->
              <label :class="{ 'out-off': outputMode !== 'to_dir' }">{{ t("output.dir") }}
                <div class="withbtn">
                  <input v-model="outputDir" :disabled="outputMode !== 'to_dir'" :placeholder="t('output.dirPh')" />
                  <button class="mini" :disabled="busy || outputMode !== 'to_dir'" @click="doPickOutputDir">{{ t("output.pick") }}</button>
                </div>
              </label>
            </div>
          </section>
        </div>

        <section class="panel">
          <h2>{{ t("step.rules") }}</h2>
          <div class="rules">
            <label>{{ t("rule.pattern") }}
              <input v-model="rule.pattern" :placeholder="t('rule.patternPh')" />
            </label>
            <label>{{ t("rule.replacement") }}
              <input v-model="rule.replacement" :placeholder="t('rule.replacementPh')" />
            </label>
            <label>{{ t("rule.scope") }}
              <select v-model="rule.scope">
                <option value="name">{{ t("rule.scope.name") }}</option>
                <option value="full">{{ t("rule.scope.full") }}</option>
                <option value="ext">{{ t("rule.scope.ext") }}</option>
              </select>
            </label>
            <label>{{ t("rule.filter") }}
              <input v-model="rule.filter" :placeholder="t('rule.filterPh')" />
            </label>
            <label>{{ t("rule.seqStart") }}
              <input type="number" v-model.number="rule.seqStart" min="0" />
            </label>
            <label>{{ t("rule.seqWidth") }}
              <input type="number" v-model.number="rule.seqWidth" min="0" max="12" />
            </label>
          </div>
          <div class="row">
            <label class="check"><input type="checkbox" v-model="rule.caseInsensitive" /> {{ t("rule.ignoreCase") }}</label>
            <label class="check"><input type="checkbox" v-model="rule.matchAll" /> {{ t("rule.matchAll") }}</label>
          </div>
          <div class="row presets-row">
            <span class="presets">{{ t("rule.presets") }}</span>
            <button
              v-for="p in PRESETS" :key="p.key"
              class="mini preset-btn" :class="{ 'preset-clear': p.key === 'clear' }"
              :title="t(`preset.${p.key}`)"
              @click="applyPreset(p)"
            >{{ t(`preset.${p.key}`) }}</button>
          </div>
          <div class="row tpl-row">
            <span class="presets">{{ t("tpl.title") }}</span>
            <select v-model="selectedTemplateId" class="tpl-select" :title="t('tpl.selectTitle')">
              <option value="">{{ t("tpl.none") }}</option>
              <option v-for="tpl in templates" :key="tpl.id" :value="tpl.id">{{ tpl.name }}</option>
            </select>
            <button class="mini" :disabled="!selectedTemplateId" @click="applySelectedTemplate">{{ t("tpl.apply") }}</button>
            <button class="mini" :disabled="!selectedTemplateId" @click="deleteSelectedTemplate">{{ t("tpl.delete") }}</button>
            <button class="mini" @click="saveCurrentTemplate">{{ t("tpl.save") }}</button>
            <button class="mini" @click="importTemplatesFromFile">{{ t("tpl.import") }}</button>
            <button class="mini" @click="exportTemplatesToFile">{{ t("tpl.export") }}</button>
          </div>
          <p class="hint" v-html="t('rule.vars')"></p>
          <p v-if="previewError" class="ruleerr">{{ t("rule.errPrefix") }}{{ previewError }}</p>
          <p v-if="ruleDirty" class="ruleerr">{{ t("rule.dirty") }}</p>
        </section>

        <section class="panel actions">
          <div class="row">
            <button :disabled="busy || !hasSelection" :title="nextStep" @click="doScan">{{ t("action.scan") }}</button>
            <button :disabled="busy || !jobId || !!previewError" :title="nextStep" @click="doPlan">{{ t("action.plan") }}</button>
            <button :disabled="busy || !plan || ruleDirty" :title="nextStep" @click="doExecute">{{ t("action.execute") }}</button>
            <button :disabled="!jobId || (!paused && progress?.phase !== 'executing')" @click="togglePause">
              {{ paused ? t("action.resume") : t("action.pause") }}
            </button>
            <button :disabled="!jobId" :title="t('action.cancelTitle')" @click="doCancel">{{ t("action.cancel") }}</button>
            <span class="spacer" />
            <button :disabled="busy || !jobId" :title="t('action.undoTitle')" @click="doUndo">{{ t("action.undo") }}</button>
            <button v-if="undoPending" :disabled="busy" class="danger" :title="t('action.undoSafeTitle')" @click="confirmUndo">
              {{ t("action.undoSafe", { n: precheck?.okCount ?? 0 }) }}
            </button>
            <button :disabled="busy || !jobId" :title="t('action.conflictsTitle')" @click="toggleConflicts">
              {{ conflictOpen ? t("action.conflictsClose") : t("action.conflicts") }}
            </button>
          </div>
          <p class="hint">{{ t("actions.next") }}<b>{{ nextStep }}</b> ｜ {{ t("actions.job") }}：<code>{{ jobId || "—" }}</code>
            ｜ {{ t("actions.status") }}：<code>{{ statusLabel(currentJob?.status ?? "") }}</code>
            ｜ {{ t("actions.undoable") }}：<code>{{ currentJob?.undoAvailable ? t("common.yes") : t("common.no") }}</code></p>
        </section>

        <!-- 上下文报告：不随状态栏固定，跟着主内容滚动 -->
        <div v-if="precheck || plan" class="notices">
          <p v-if="precheck" class="notice" v-html="precheckHtml"></p>
          <p v-if="plan" class="notice">{{ planNotice }}</p>
        </div>

        <!-- M3 收尾 A：撤销复核冲突明细分页（后端分页，前端只收当前页） -->
        <section v-if="conflictOpen" class="panel conflicts">
          <div class="panel-head">
            <h2>{{ t("conflicts.title", { n: conflictTotal }) }}</h2>
            <span class="spacer" />
            <button class="mini" :disabled="conflictBusy || conflictCursors.length < 2" @click="conflictPrev">{{ t("conflicts.prev") }}</button>
            <span class="hint">{{ t("conflicts.page", { p: conflictCursors.length, n: conflictRows.length }) }}</span>
            <button class="mini" :disabled="conflictBusy || conflictNextCursor === null" @click="conflictNext">{{ t("conflicts.next") }}</button>
            <button class="mini" :disabled="conflictBusy" @click="closeConflicts">{{ t("conflicts.close") }}</button>
          </div>
          <div class="thead"><span>{{ t("entries.orig") }}</span><span>{{ t("entries.dest") }}</span><span>{{ t("entries.status") }}</span></div>
          <div class="conflict-list scroller">
            <div v-for="r in conflictRows" :key="r.id" class="crow">
              <span :title="r.srcPath" class="ellipsis">{{ r.srcName }}</span>
              <span :title="r.dstPath" class="ellipsis">{{ r.dstName || "—" }}</span>
              <span class="badge" :class="conflictCode(r)">{{ statusLabel(conflictCode(r)) }}</span>
            </div>
            <p v-if="!conflictRows.length && !conflictBusy" class="empty">{{ t("conflicts.empty") }}</p>
          </div>
          <p class="hint">{{ t("conflicts.hint") }}</p>
        </section>

        <div class="split">
          <section class="panel">
            <div class="panel-head">
              <h2>{{ t("history.title") }}</h2>
              <button class="mini" @click="refreshHistory">{{ t("history.refresh") }}</button>
            </div>
            <div class="history scroller">
              <div v-for="j in history" :key="j.id" class="hist-row" :class="{ active: j.id === jobId }" @click="selectHistory(j)">
                <span class="mono">{{ j.id.slice(0, 14) }}</span>
                <span class="badge" :class="j.status">{{ statusLabel(j.status) }}</span>
                <span>{{ t("history.count", { n: j.total }) }}</span>
                <span>{{ j.outputMode === 'to_dir' ? t('history.copyMode') : t('history.inPlaceMode') }}</span>
                <span v-if="j.undoAvailable" class="ok">{{ t("history.canUndo") }}</span>
                <span class="spacer" />
                <button
                  class="mini hist-btn" :disabled="busy"
                  :title="t('history.exportTitle')" :aria-label="t('history.exportTitle')"
                  @click.stop="exportHistory(j)"
                ><Download :size="13" :stroke-width="1.9" /></button>
                <button
                  v-if="j.status === 'interrupted'" class="mini hist-btn" :disabled="busy"
                  :title="t('history.resumeTitle')" :aria-label="t('history.resumeTitle')"
                  @click.stop="resumeJob(j)"
                ><RotateCcw :size="13" :stroke-width="1.9" /></button>
              </div>
              <p v-if="!history.length" class="empty">{{ t("history.empty") }}</p>
            </div>
            <p class="hint hist-hint">{{ t("history.hint") }}</p>
          </section>

          <section class="panel">
            <div class="panel-head">
              <h2>{{ t("entries.title", { n: rowTotal }) }}</h2>
              <select v-model="statusFilter" @change="loadEntries(true)">
                <option value="">{{ t("entries.filterAll") }}</option>
                <option v-for="s in STATUS_CODES" :key="s" :value="s">{{ statusLabel(s) }}</option>
              </select>
            </div>
            <div class="thead thead-r"><span>{{ t("entries.orig") }}</span><span>{{ t("entries.dest") }}</span><span>{{ t("entries.status") }}</span></div>
            <div class="vlist scroller" :style="{ height: VIEW_H + 'px' }" @scroll="onScroll">
              <div :style="{ height: rows.length * ROW_H + 'px', position: 'relative' }">
                <div v-for="x in visibleRows" :key="x.r.id" class="vrow" :style="{ top: x.i * ROW_H + 'px', height: ROW_H + 'px' }">
                  <span :title="x.r.srcPath" class="ellipsis">{{ x.r.srcName }}</span>
                  <span :title="x.r.dstPath" class="ellipsis">{{ rowDst(x.r) }}</span>
                  <span class="badge" :class="rowBadge(x.r)">{{ statusLabel(rowBadge(x.r)) }}</span>
                </div>
              </div>
            </div>
            <button v-if="nextCursor !== null" class="mini load-more" @click="loadEntries(false)">{{ t("entries.loadMore") }}</button>
          </section>
        </div>
      </div>
    </div>

    <!-- 固定在底部的状态栏：原「流式进度」并入此处，标题改为「进度」 -->
    <footer class="statusbar">
      <div class="sb-progress">
        <div class="sb-head">
          <span class="sb-title">{{ t("progress.title") }}</span>
          <div class="sb-bars">
            <div class="sb-line">
              <span class="sb-name">{{ t("progress.files") }}</span>
              <div class="bar"><i :style="{ width: pctFiles + '%' }" /></div>
              <span class="sb-val">{{ progress?.done ?? 0 }} / {{ progress?.total ?? 0 }} · {{ pctFiles.toFixed(1) }}%</span>
            </div>
            <div class="sb-line">
              <span class="sb-name">{{ t("progress.bytes") }}</span>
              <div class="bar bytes"><i :style="{ width: pctBytes + '%' }" /></div>
              <span class="sb-val">{{ fmtBytes(progress?.bytesDone ?? 0) }} / {{ fmtBytes(progress?.bytesTotal ?? 0) }} · {{ pctBytes.toFixed(1) }}%</span>
            </div>
          </div>
        </div>
        <div class="sb-stats">
          <span>{{ t("progress.phase") }} <b>{{ phaseText }}</b></span>
          <span>{{ t("progress.fileRate") }} <b>{{ (progress?.speed ?? 0).toFixed(0) }} /s</b></span>
          <span>{{ t("progress.byteRate") }} <b>{{ fmtBytes(progress?.bytesPerSec ?? 0) }}/s</b></span>
          <span>{{ t("progress.eta") }} <b>{{ fmtEta(progress?.etaMs ?? 0) }}</b></span>
          <span>{{ t("progress.elapsed") }} <b>{{ ((progress?.elapsedMs ?? 0) / 1000).toFixed(1) }}s</b></span>
          <span>{{ t("progress.failedSkipped") }} <b>{{ progress?.failed ?? 0 }} / {{ progress?.skipped ?? 0 }}</b></span>
        </div>
      </div>
      <div class="sb-msg" :class="{ err: lastMsgErr }" :title="lastMsg">{{ lastMsg || t("msg.ready") }}</div>
    </footer>
  </div>
</template>

<style>
:root {
  font-family: -apple-system, "Segoe UI", "Microsoft YaHei", sans-serif;
  font-size: 14px; color: #1c2626; background: #f2f6f5;
}
* { box-sizing: border-box; }
html, body, #app { height: 100%; margin: 0; overflow: hidden; }
/* 滚动条：细条圆角；不滚动时隐藏，滚动中或悬停在滚动区域时淡入 */
:root { scrollbar-width: thin; scrollbar-color: transparent transparent; }
::-webkit-scrollbar { width: 8px; height: 8px; }
::-webkit-scrollbar-track { background: transparent; }
::-webkit-scrollbar-thumb {
  background-color: transparent;
  border: 2px solid transparent;
  border-radius: 8px;
  background-clip: padding-box;
  transition: background-color .22s ease;
}
html.scrolling, html.scrolling body { scrollbar-color: rgba(15, 95, 92, .42) transparent; }
html.scrolling ::-webkit-scrollbar-thumb,
html.scrolling::-webkit-scrollbar-thumb,
.scroller:hover::-webkit-scrollbar-thumb,
.scroller:focus-within::-webkit-scrollbar-thumb { background-color: rgba(15, 95, 92, .42); }
html.scrolling ::-webkit-scrollbar-thumb:hover,
html.scrolling::-webkit-scrollbar-thumb:hover { background-color: rgba(15, 95, 92, .7); }

/* 三段式布局：标题栏固定 / 内容区独立滚动 / 状态栏固定 */
.shell { display: flex; flex-direction: column; height: 100vh; overflow: hidden; }
.app-scroll { flex: 1; min-height: 0; overflow-y: auto; }
.app { max-width: 1600px; margin: 0 auto; padding: 12px 14px; }
/* 首行：选择对象 / 输出去向 各占一半；内部字段半宽下改竖排 */
.top-split { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; align-items: stretch; margin-bottom: 14px; }
.top-split > .panel { margin-bottom: 0; min-width: 0; }
.top-split .grid { grid-template-columns: 1fr; }
header { display: flex; justify-content: space-between; align-items: flex-start; margin-bottom: 16px; }
h1 { margin: 0; font-size: 22px; color: #0f5f5c; }
h1 .tag { font-size: 12px; font-weight: 500; color: #0f5f5c; background: #d8eeeb; border: 1px solid #bfd8d6; border-radius: 10px; padding: 2px 8px; vertical-align: middle; }
h1 .ver { font-size: 11px; font-weight: 500; color: #9ab0ac; letter-spacing: .2px; margin-left: 9px; vertical-align: middle; }
.sub { margin: 4px 0 0; color: #617473; font-size: 12px; }
h2 { margin: 0 0 12px; font-size: 14px; font-weight: 600; color: #0f5f5c; }
/* header 运行状态徽章：四种色调（就绪/运行/完成/异常），圆点在运行态呼吸 */
.hdr-state { display: inline-flex; align-items: center; gap: 7px; margin-top: 3px; padding: 5px 12px; border-radius: 999px; font-size: 12px; font-weight: 600; letter-spacing: .2px; white-space: nowrap; border: 1px solid #cfe3df; background: #eef6f4; color: #46706c; }
.hdr-dot { width: 7px; height: 7px; border-radius: 50%; background: currentColor; flex: 0 0 auto; }
.hdr-state.run { background: #dff0e6; border-color: #a8d5bd; color: #0e6b47; }
.hdr-state.run .hdr-dot { animation: hdr-breathe 1.4s ease-in-out infinite; }
.hdr-state.done { background: #e4eef8; border-color: #b9d1e8; color: #315f8a; }
.hdr-state.warn { background: #fbeae4; border-color: #eec3b5; color: #a33d24; }
@keyframes hdr-breathe { 0%,100% { opacity: 1; box-shadow: 0 0 0 0 rgba(14,107,71,.45); } 50% { opacity: .65; box-shadow: 0 0 0 4px rgba(14,107,71,0); } }
@media (prefers-reduced-motion: reduce) { .hdr-state.run .hdr-dot { animation: none; } }
.panel { background: #fff; border: 1px solid #dce7e5; border-radius: 10px; padding: 14px 16px; margin-bottom: 14px; }
.grid { display: grid; grid-template-columns: 1.2fr 1fr 1.2fr; gap: 12px; }
.rules { display: grid; grid-template-columns: 1fr 1fr; gap: 10px 12px; }
.rules > label:nth-child(5), .rules > label:nth-child(6) { max-width: 50%; }
.withbtn { display: flex; gap: 6px; }
.withbtn input { flex: 1; min-width: 0; }
/* 输出目录常驻但禁用：不隐藏、不改变卡片高度，只做视觉弱化 */
.out-off { opacity: .48; }
.out-off input:disabled { background: #f2f6f5; cursor: not-allowed; }
label { display: flex; flex-direction: column; gap: 5px; font-size: 12px; color: #4c605e; }
input[type="text"], input:not([type]), input[type="number"], select { border: 1px solid #c9dad8; border-radius: 6px; padding: 7px 9px; font-size: 13px; color: #1c2626; background: #fbfdfd; outline: none; }
input:focus, select:focus { border-color: #0f5f5c; }
.row { display: flex; align-items: center; gap: 8px; margin-top: 12px; flex-wrap: wrap; }
.check { flex-direction: row; align-items: center; gap: 6px; }
.spacer { flex: 1; }
.pickinfo { margin-top: 6px; font-size: 12px; color: #0f5f5c; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.presets { font-size: 11px; color: #718684; flex: 0 0 auto; }
.presets-row { gap: 6px; margin-top: 10px; }
.tpl-row { gap: 6px; margin-top: 8px; flex-wrap: wrap; }
.tpl-select { min-width: 130px; max-width: 190px; flex: 0 1 auto; }
.preset-btn { padding: 4px 9px; }
.preset-clear { color: #718684; border-color: #c9dad8; }
.ruleerr { margin: 10px 0 0; font-size: 12px; color: #b4462f; background: #fbece7; border-left: 3px solid #b4462f; padding: 7px 10px; border-radius: 4px; }
button { border: 1px solid #0f5f5c; background: #0f5f5c; color: #fff; border-radius: 6px; padding: 7px 14px; font-size: 13px; cursor: pointer; }
button:hover:not(:disabled) { background: #147570; }
button:disabled { opacity: .45; cursor: not-allowed; }
button.danger { background: #b4462f; border-color: #b4462f; }
button.mini { padding: 4px 10px; font-size: 12px; background: #fff; color: #0f5f5c; }
.actions { background: #f7fbfa; }
.hint { margin: 10px 0 0; font-size: 12px; color: #617473; }
code { font-family: Consolas, monospace; background: #eef5f4; padding: 1px 5px; border-radius: 4px; }
.bar { height: 9px; background: #e5efed; border-radius: 5px; overflow: hidden; }
.bar i { display: block; height: 100%; background: #0f5f5c; transition: width .12s linear; }
.bar.bytes i { background: #3d9089; }
.notice { margin: 0 0 12px; font-size: 12px; background: #eef7f5; border-left: 3px solid #0f5f5c; padding: 8px 10px; border-radius: 4px; }
.notices { margin-bottom: 2px; }
.split { display: grid; grid-template-columns: 1fr 1.35fr; gap: 14px; }
.panel-head { display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px; }
.panel-head h2 { margin: 0; }
.panel-head select { padding: 4px 8px; font-size: 12px; }
.history { max-height: 300px; overflow: auto; }
.hist-row { display: flex; gap: 8px; align-items: center; padding: 6px 4px; border-bottom: 1px solid #eef3f2; font-size: 12px; cursor: pointer; }
.hist-row:hover { background: #f3f9f8; }
.hist-row.active { background: #e3f2ef; }
.mono { font-family: Consolas, monospace; color: #4c605e; }
.ok { color: #0f7a54; font-weight: 600; }
.badge { font-size: 11px; padding: 1px 6px; border-radius: 4px; background: #eef2f1; color: #4c605e; white-space: nowrap; }
.badge.done, .badge.completed, .badge.planned { background: #dff0e6; color: #0f6b48; }
.badge.failed, .badge.conflict, .badge.UNDO_TARGET_MODIFIED, .badge.UNDO_SOURCE_OCCUPIED { background: #fbe7e1; color: #a33d24; }
.badge.skipped { background: #f1eee0; color: #8a7226; }
.badge.undone { background: #e6e9f5; color: #46528c; }
.thead, .vrow { display: grid; grid-template-columns: 1fr 1fr 130px; gap: 8px; align-items: center; }
/* 表头下不再画亮线（暗色主题下原 #e5efed 会刺眼）；保留 1px 占位，布局高度不变 */
.thead { font-size: 11px; color: #718684; padding: 0 8px 5px; border-bottom: 1px solid transparent; }
.vlist { overflow-y: auto; border: 1px solid #e5efed; border-radius: 6px; margin-top: 4px; }
.vrow { position: absolute; left: 0; right: 0; padding: 0 8px; font-size: 12px; border-bottom: 1px solid #f2f6f5; }
.vrow:nth-child(even) { background: #fafcfc; }
/* 状态徽章按内容包裹，不再被 grid 拉伸到整列宽（原 130px 显得过长） */
.vrow .badge { justify-self: end; width: fit-content; }
/* 条目预览表头：状态列表头与徽章同为 130px 列，文字一并靠右对齐 */
.thead-r > span:last-child { justify-self: end; text-align: right; }
/* 「加载下一页」居右，与上方可滚动列表留 3px 间距 */
.load-more { display: block; margin: 3px 0 0 auto; }
.ellipsis { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
/* 历史行右侧的紧凑操作按钮：不撑高行、不触发整行选中 */
button.hist-btn { display: inline-flex; align-items: center; justify-content: center; flex: 0 0 auto; padding: 3px 7px; line-height: 1; }
.hist-row .spacer { min-width: 4px; }
.hist-hint { margin-top: 8px; }

/* M3 收尾 A：冲突明细面板，列宽与 .thead 一致，复用 .badge 状态码配色 */
.conflicts .panel-head { gap: 8px; }
.conflict-list { max-height: 220px; overflow-y: auto; border: 1px solid #e5efed; border-radius: 6px; margin-top: 4px; }
.crow { display: grid; grid-template-columns: 1fr 1fr 130px; gap: 8px; align-items: center; padding: 5px 8px; font-size: 12px; border-bottom: 1px solid #f2f6f5; }
.crow:nth-child(even) { background: #fafcfc; }
.empty { color: #91a3a1; font-size: 12px; text-align: center; padding: 20px; }

/* 底部状态栏：进度 + 最近一条状态，不显示日志路径（已移到设置页） */
.statusbar {
  flex: 0 0 auto;
  display: flex; align-items: center; gap: 16px;
  padding: 8px 14px;
  font-size: 12px; color: #617473;
  background: #fff; border-top: 1px solid #dce7e5;
}
.sb-progress { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 7px; }
.sb-head { display: flex; align-items: center; gap: 12px; }
.sb-title { flex: 0 0 auto; font-size: 12px; font-weight: 600; color: #0f5f5c; }
.sb-bars { flex: 1; min-width: 0; display: grid; grid-template-columns: 1fr 1fr; gap: 4px 18px; }
.sb-line { display: flex; align-items: center; gap: 8px; min-width: 0; font-size: 11.5px; color: #4c605e; }
.sb-name { flex: 0 0 auto; min-width: 42px; }
.sb-line .bar { flex: 1; min-width: 36px; height: 7px; }
.sb-val { flex: 0 0 auto; min-width: 140px; text-align: right; font-variant-numeric: tabular-nums; }
.sb-stats { display: flex; flex-wrap: wrap; gap: 3px 16px; font-size: 11px; color: #718684; }
.sb-stats b { color: #1c2626; font-weight: 600; }
.sb-msg {
  flex: 0 0 230px; max-width: 24%; padding-left: 12px;
  border-left: 1px solid #e5efed;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-size: 11.5px; color: #617473;
}
.sb-msg.err { color: #b4462f; }

@media (max-width: 900px) {
  .grid, .split, .rules, .top-split { grid-template-columns: 1fr; }
  .rules > label:nth-child(5), .rules > label:nth-child(6) { max-width: none; }
  .sb-bars { grid-template-columns: 1fr; }
  .sb-msg { display: none; }
}
/* 暗色由 theme.ts 写在 <html data-theme="dark"> 上驱动，跟随系统 / 手动指定共用同一套配色 */
html[data-theme="dark"] { color: #dce8e6; background: #14201f; }
html[data-theme="dark"] .panel { background: #1b2a29; border-color: #2c403e; }
html[data-theme="dark"] .actions { background: #172624; }
html[data-theme="dark"] h1, html[data-theme="dark"] h2 { color: #6fd0c4; }
html[data-theme="dark"] .hdr-state { background: #1d302e; border-color: #37524f; color: #9dbbb7; }
html[data-theme="dark"] .hdr-state.run { background: #173a2d; border-color: #2c6047; color: #7fd6a8; }
html[data-theme="dark"] .hdr-state.done { background: #1c2f42; border-color: #345474; color: #93bfe8; }
html[data-theme="dark"] .hdr-state.warn { background: #402722; border-color: #704034; color: #f0a08c; }
html[data-theme="dark"] .sub, html[data-theme="dark"] .hint, html[data-theme="dark"] .bar-label, html[data-theme="dark"] .thead { color: #92aaa7; }
html[data-theme="dark"] input:not([type]), html[data-theme="dark"] input[type="text"], html[data-theme="dark"] input[type="number"], html[data-theme="dark"] select { background: #162423; color: #dce8e6; border-color: #37524f; }
html[data-theme="dark"] .bar { background: #2a3c3a; }
html[data-theme="dark"] .hist-row, html[data-theme="dark"] .vrow { border-color: #2a3c3a; }
html[data-theme="dark"] .conflict-list, html[data-theme="dark"] .crow { border-color: #2a3c3a; }
html[data-theme="dark"] .crow:nth-child(even) { background: #203230; }
html[data-theme="dark"] .hist-row:hover, html[data-theme="dark"] .vrow:nth-child(even) { background: #203230; }
html[data-theme="dark"] .notice { background: #1d3431; }
html[data-theme="dark"] .ruleerr { background: #3a231f; color: #f0a08c; }
html[data-theme="dark"] code { background: #263b39; }
html[data-theme="dark"] .badge { background: #2a3c3a; color: #b9cfcc; }
html[data-theme="dark"] .badge.done, html[data-theme="dark"] .badge.completed, html[data-theme="dark"] .badge.planned { background: #214437; color: #7fd6a8; }
html[data-theme="dark"] .badge.failed, html[data-theme="dark"] .badge.conflict, html[data-theme="dark"] .badge.UNDO_TARGET_MODIFIED, html[data-theme="dark"] .badge.UNDO_SOURCE_OCCUPIED { background: #4a2a24; color: #f0a08c; }
html[data-theme="dark"] .badge.skipped { background: #3b3520; color: #e3c877; }
html[data-theme="dark"].scrolling, html[data-theme="dark"].scrolling body { scrollbar-color: rgba(111, 208, 196, .38) transparent; }
html[data-theme="dark"].scrolling ::-webkit-scrollbar-thumb,
html[data-theme="dark"].scrolling::-webkit-scrollbar-thumb,
html[data-theme="dark"] .scroller:hover::-webkit-scrollbar-thumb,
html[data-theme="dark"] .scroller:focus-within::-webkit-scrollbar-thumb { background-color: rgba(111, 208, 196, .38); }
html[data-theme="dark"].scrolling ::-webkit-scrollbar-thumb:hover,
html[data-theme="dark"].scrolling::-webkit-scrollbar-thumb:hover { background-color: rgba(111, 208, 196, .62); }
html[data-theme="dark"] h1 .tag { background: #1d3431; border-color: #33504c; color: #8fded2; }
html[data-theme="dark"] h1 .ver { color: #7fa5a0; }
html[data-theme="dark"] button.mini { background: #1b2a29; color: #8fded2; border-color: #37524f; }
html[data-theme="dark"] button.mini:hover:not(:disabled) { background: #203230; }
html[data-theme="dark"] .preset-clear { color: #92aaa7; border-color: #37524f; }
html[data-theme="dark"] .vlist { border-color: #2a3c3a; }
html[data-theme="dark"] .out-off input:disabled { background: #14201f; }
html[data-theme="dark"] .hist-row.active { background: #1d3431; }
html[data-theme="dark"] .pickinfo { color: #8fded2; }
html[data-theme="dark"] .statusbar { background: #1b2a29; border-top-color: #2c403e; }
html[data-theme="dark"] .sb-stats, html[data-theme="dark"] .sb-line { color: #92aaa7; }
html[data-theme="dark"] .sb-stats b { color: #dce8e6; }
html[data-theme="dark"] .sb-title { color: #6fd0c4; }
html[data-theme="dark"] .sb-msg { color: #92aaa7; border-left-color: #2c403e; }
html[data-theme="dark"] .sb-msg.err { color: #f0a08c; }
</style>
