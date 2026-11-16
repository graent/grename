/**
 * 本地二维码真实性校验（离线版）。
 *
 * 图片随应用打包，唯一真源为项目根 imgs/{wx,zfb,gzh}.jpg。
 * 运行时只从同源本地资源读取字节，用 Web Crypto 计算 SHA-256，
 * 与随版本固化的期望值逐字比对；只有完全一致才允许展示真码，
 * 否则保持占位图——防止安装目录里的二维码被替换伪造。
 *
 * 不再请求任何远程图片，也不再依赖 /community 返回的 qrs；
 * 任何异常（文件缺失、读取失败、crypto 不可用）都返回 false，
 * 由调用方保持占位，绝不因为校验链路异常而放行一个未验证的码。
 */
import wxUrl from "../../imgs/wx.jpg?url";
import zfbUrl from "../../imgs/zfb.jpg?url";
import gzhUrl from "../../imgs/gzh.jpg?url";

/** platform → 本地资源 URL + 随版本固化的 SHA-256（图片变更时必须同步刷新）。 */
export const LOCAL_QRS: Record<string, { url: string; sha256: string }> = {
  wechat: { url: wxUrl, sha256: "3ce1b62613eeb1aa3484e383ad2156d2b1c348ab6166c751a3110b8bea7512b8" },
  alipay: { url: zfbUrl, sha256: "8933ffebaf2803991b1a614c94aeed86f7c979d17728bc8d50d8c8af174c17c1" },
  mp: { url: gzhUrl, sha256: "4eb7e2bf4e12ecf8a6ca976aa7fb8b29540fcbf1fe4302cd989b184438b67d56" },
};

export async function verifyQr(url: string, expectedSha256: string): Promise<boolean> {
  // 只接受同源 / 相对路径：显式拒绝远程 URL，杜绝回退到网络取图。
  if (!url || !expectedSha256 || /^https?:\/\//i.test(url)) return false;
  const res = await fetch(url, { cache: "no-store" });
  if (!res.ok) return false;
  const bytes = await res.arrayBuffer();
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  const hex = Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  return hex.toLowerCase() === expectedSha256.toLowerCase();
}
