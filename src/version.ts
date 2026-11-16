/**
 * 应用版本号的单一来源（前端）。
 *
 * 直接读 code/package.json，避免各组件硬编码后与发版工具链分叉：
 * done/tools/m4d_release.py bump-version 会同步
 * tauri.conf.json / package.json / Cargo.toml 三处，前端展示自动跟随 package.json，
 * 不需要增加第四处需要手工维护的版本号。
 *
 * tsconfig.json 已开 resolveJsonModule；Vite 会把该 JSON 模块内联进产物。
 */
import pkg from "../package.json";

export const APP_VERSION: string = pkg.version;
