/**
 * 后端公开 API 接入层（M4-M）。
 *
 * 后端：ThinkPHP 多应用 `app/grename`，路由见其 route/route.php。
 * baseUrl：https://gr.graent.cn/grename（所有路径在此基址下拓展）。
 *
 * 设计约束：
 *  - 只走公开只读/提交端点：update（更新检查）、feedback（提交）、community（收款码 + 捐赠记录）；
 *  - 后端已开放 CORS（AllowCrossDomain），TAURI WebView 内原生 fetch 即可，无需新增 Rust 命令 / 依赖；
 *  - 所有网络异常归一化为普通 Error，调用方决定降级（保持占位 / 空态），绝不让界面因网络失败报错；
 *  - 截图上传接口后端暂未提供，本模块不做任何文件上传。
 */
import { APP_VERSION } from "./version";

export const API_BASE = "https://gr.graent.cn/grename";

/** GET /grename/update → { latest, url, notes, force } */
export interface UpdateInfo {
  latest: string;
  url: string;
  notes: string[];
  force: number;
}
/** GET /grename/community → { donations, groups, feedbacks, qrs } */
export interface QrItem {
  platform: string;
  url: string;
  sha256: string;
  payee: string;
  account: string;
}
export interface DonationItem {
  name: string;
  amount: number;
  date: string;
  message: string;
  method: string;
}
export interface CommunityInfo {
  donations: DonationItem[];
  groups: Array<{ type: string; name: string; note: string; hidden: boolean }>;
  feedbacks: Array<{ name: string; content: string; date: string; reply: string }>;
  qrs: QrItem[];
}
export interface FeedbackPayload {
  name: string;
  type: "bug" | "suggest" | "other";
  contact: string;
  content: string;
}
export interface FeedbackResult {
  ok: boolean;
  message: string;
  id?: number;
}

/** 前端反馈类型 → 后端枚举（后端只认 bug / suggest / other）。 */
export function mapFeedbackType(t: string): FeedbackPayload["type"] {
  return t === "bug" || t === "other" ? t : "suggest";
}

/** 统一 GET JSON：超时 + HTTP 状态 + 解析失败都归一化为 Error。 */
async function getJson<T>(path: string, timeoutMs = 8000): Promise<T> {
  const ctrl = new AbortController();
  const timer = window.setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const res = await fetch(`${API_BASE}${path}`, {
      signal: ctrl.signal,
      headers: { Accept: "application/json" },
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return (await res.json()) as T;
  } finally {
    window.clearTimeout(timer);
  }
}

export function fetchUpdate(): Promise<UpdateInfo> {
  return getJson<UpdateInfo>("/update");
}

export function fetchCommunity(): Promise<CommunityInfo> {
  return getJson<CommunityInfo>("/community");
}

/**
 * POST /grename/feedback —— 只提交文本字段，截图暂不上传（后端无上传端点）。
 *
 * 用 application/x-www-form-urlencoded 而非 JSON：JSON 会触发 CORS 预检，
 * 而实测该端点 OPTIONS 返回 204 却不带 Access-Control-Allow-* 头，
 * WebView 会因此拦下真实请求。表单属于「简单请求」，不产生预检；
 * 后端 Feedback::submit() 用 Request::param()，表单与 JSON 都能解析。
 * （实测 POST 表单返回 200 + Access-Control-Allow-Origin。）
 */
export async function postFeedback(payload: FeedbackPayload): Promise<FeedbackResult> {
  const ctrl = new AbortController();
  const timer = window.setTimeout(() => ctrl.abort(), 8000);
  try {
    const body = new URLSearchParams({
      name: payload.name,
      type: payload.type,
      contact: payload.contact,
      content: payload.content,
    });
    const res = await fetch(`${API_BASE}/feedback`, {
      method: "POST",
      signal: ctrl.signal,
      headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: "application/json" },
      body: body.toString(),
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return (await res.json()) as FeedbackResult;
  } finally {
    window.clearTimeout(timer);
  }
}

/** 比较 SemVer：a>b 返回 1，a<b 返回 -1，相等返回 0。非数字段按 0 处理。 */
export function compareSemver(a: string, b: string): number {
  const pa = a.replace(/^v/i, "").split(".").map((x) => parseInt(x, 10) || 0);
  const pb = b.replace(/^v/i, "").split(".").map((x) => parseInt(x, 10) || 0);
  for (let i = 0; i < 3; i++) {
    const x = pa[i] ?? 0;
    const y = pb[i] ?? 0;
    if (x > y) return 1;
    if (x < y) return -1;
  }
  return 0;
}

/** 当前本地版本（供更新检查比对，避免调用方再单独 import）。 */
export { APP_VERSION };

/** 把后端 notes（字符串数组）拼成展示文本。 */
export function formatNotes(notes: string[]): string {
  return (notes ?? []).filter((s) => s && s.trim()).join("\n");
}
