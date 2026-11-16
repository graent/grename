/**
 * M0 补充项：UI 帧率 rAF 埋点（设计文档 §3.1 / §10 / §12.1 第 7 项）。
 *
 * 判据：**执行期间 ≥ 55 fps、无 > 100 ms 长任务，且与任务规模 N 无关**。
 * 本模块只负责「可采集」，不制造数据、不代替 GUI 实测：
 *   - 帧间隔取 rAF 回调的时间戳（单调），不用 Date.now()，避免系统时钟调整；
 *   - 采样有上限，内存恒定，不随运行时长增长（§3.1）；
 *   - 每次 start(label, jobTotal) 标记任务规模，stop() 产出结构化摘要，
 *     摘要由 App.vue 写入 grename.log，并可通过 window.__grenameFrameProbe 导出 CSV；
 *   - 支持注入 raf / caf / now，便于在 Node 里用合成帧做逻辑自测（source=synthetic）。
 *
 * 注意：帧间隔 > 100 ms 既可能来自主线程长任务，也可能来自窗口失焦 / 最小化 /
 * 系统调度，因此只作为「待人工复核的可疑帧」计数，不等同于 PerformanceObserver
 * 的原生 longtask；后者可用时单独记在 nativeLongTasks 里。
 */

export interface FrameSummary {
  label: string;
  jobTotal: number;
  source: "tauri-gui" | "synthetic";
  frames: number;
  durationMs: number;
  fps: number;
  p50Ms: number;
  p95Ms: number;
  p99Ms: number;
  maxMs: number;
  /** 帧间隔 > 100ms 的可疑长任务数（含失焦等外部因素，需人工复核）。 */
  longFrames: number;
  longFrameMaxMs: number;
  /** PerformanceObserver('longtask') 捕获的原生长任务数；不支持时为 -1。 */
  nativeLongTasks: number;
  startedAt: string;
  endedAt: string;
  truncated: boolean;
}

export interface FrameProbeOptions {
  onFps?: (fps: number, summary: FrameSummary) => void;
  raf?: (cb: (ts: number) => void) => number;
  caf?: (id: number) => void;
  now?: () => number;
  maxFrames?: number;
  source?: "tauri-gui" | "synthetic";
}

const LONG_FRAME_MS = 100;
const DEFAULT_MAX_FRAMES = 20_000; // 约 5.5 分钟 @60fps，之后只累计计数不再存样本

function pct(sorted: number[], q: number): number {
  if (!sorted.length) return 0;
  const i = Math.min(sorted.length - 1, Math.round((sorted.length - 1) * q));
  return Math.round(sorted[i] * 100) / 100;
}

export function createFrameProbe(opts: FrameProbeOptions = {}) {
  const raf = opts.raf ?? ((cb: (ts: number) => void) => requestAnimationFrame(cb));
  const caf = opts.caf ?? ((id: number) => cancelAnimationFrame(id));
  const now = opts.now ?? (() => Date.now());
  const maxFrames = opts.maxFrames ?? DEFAULT_MAX_FRAMES;
  const source = opts.source ?? "tauri-gui";

  let rafId = 0;
  let running = false;
  let label = "idle";
  let jobTotal = 0;
  let startedAt = "";
  let firstTs = 0;
  let lastTs = 0;
  let lastFrameTs = 0;
  let windowStart = 0;
  let windowFrames = 0;
  let frames = 0;
  let intervals: number[] = [];
  let longFrames = 0;
  let longFrameMaxMs = 0;
  let nativeLongTasks = -1;
  let observer: { disconnect(): void } | null = null;

  function attachObserver() {
    if (source !== "tauri-gui" || typeof PerformanceObserver === "undefined") return;
    try {
      const po = new PerformanceObserver((list) => {
        nativeLongTasks = Math.max(0, nativeLongTasks) + list.getEntries().length;
      });
      po.observe({ entryTypes: ["longtask"] });
      observer = po;
      nativeLongTasks = 0;
    } catch {
      nativeLongTasks = -1; // WebView 不支持 longtask 时如实标注
      observer = null;
    }
  }

  function reset(nextLabel: string, nextTotal: number) {
    intervals = [];
    frames = 0;
    longFrames = 0;
    longFrameMaxMs = 0;
    nativeLongTasks = -1;
    firstTs = 0;
    lastTs = 0;
    lastFrameTs = 0;
    windowStart = 0;
    windowFrames = 0;
    label = nextLabel;
    jobTotal = nextTotal;
    startedAt = new Date(now()).toISOString();
  }

  function summary(): FrameSummary {
    const sorted = [...intervals].sort((a, b) => a - b);
    const durationMs = Math.round((lastTs - firstTs) * 100) / 100;
    // 首帧只建立基准、不产生间隔：N+1 帧对应 N 个间隔。
    // fps 与 truncated 都必须以「间隔数」为口径，否则前者系统性偏高 1/frames，
    // 后者因 frames = intervals.length + 1 恒成立而误报采样被截断（F-1 / F-2）。
    const intervalCount = Math.max(0, frames - 1);
    return {
      label, jobTotal, source,
      frames,
      durationMs,
      fps: durationMs > 0 ? Math.round((intervalCount * 1000 / durationMs) * 10) / 10 : 0,
      p50Ms: pct(sorted, 0.5),
      p95Ms: pct(sorted, 0.95),
      p99Ms: pct(sorted, 0.99),
      maxMs: pct(sorted, 1),
      longFrames,
      longFrameMaxMs: Math.round(longFrameMaxMs * 100) / 100,
      nativeLongTasks,
      startedAt,
      endedAt: new Date(now()).toISOString(),
      truncated: intervalCount > intervals.length,
    };
  }

  function tick(ts: number) {
    if (!running) return;
    if (!firstTs) {
      firstTs = ts;
      windowStart = ts;
    }
    if (lastFrameTs > 0) {
      const dt = ts - lastFrameTs;
      if (intervals.length < maxFrames) intervals.push(dt);
      if (dt > LONG_FRAME_MS) {
        longFrames++;
        longFrameMaxMs = Math.max(longFrameMaxMs, dt);
      }
    }
    lastFrameTs = ts;
    lastTs = ts;
    frames++;
    windowFrames++;
    // 每秒回调一次瞬时 fps，只更新一个数字，不逐帧触发 Vue 更新。
    if (ts - windowStart >= 1000) {
      const s = summary();
      opts.onFps?.(Math.round((windowFrames * 1000 / (ts - windowStart)) * 10) / 10, s);
      windowFrames = 0;
      windowStart = ts;
    }
    rafId = raf(tick);
  }

  function start(nextLabel = "idle", nextTotal = 0) {
    if (running) stop();
    reset(nextLabel, nextTotal);
    attachObserver();
    running = true;
    rafId = raf(tick);
  }

  function stop(): FrameSummary {
    if (!running) return summary();
    running = false;
    if (rafId) caf(rafId);
    rafId = 0;
    observer?.disconnect();
    observer = null;
    return summary();
  }

  /** 逐帧明细，供 §12.3 ⑥ GUI 手工回归时导出复核，不上界面。
   *  index 为「间隔序号」：第 1 个间隔结束于第 2 帧，tsMs 为该帧的时间戳。 */
  function csv(): string {
    const lines = ["index,tsMs,intervalMs,longFrame"];
    let ts = firstTs;
    for (let i = 0; i < intervals.length; i++) {
      ts += intervals[i];
      lines.push(`${i + 1},${Math.round(ts * 100) / 100},${Math.round(intervals[i] * 100) / 100},${intervals[i] > LONG_FRAME_MS ? 1 : 0}`);
    }
    return lines.join("\n");
  }

  return { start, stop, snapshot: summary, csv };
}

export type FrameProbe = ReturnType<typeof createFrameProbe>;

/** 挂调试接口；GUI 手工回归时可在开发者工具里直接取数。 */
export function installFrameProbeDebug(probe: FrameProbe) {
  if (typeof window === "undefined") return;
  (window as unknown as Record<string, unknown>).__grenameFrameProbe = {
    snapshot: () => probe.snapshot(),
    stop: () => probe.stop(),
    csv: () => probe.csv(),
  };
}
