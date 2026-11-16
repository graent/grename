/**
 * 设置菜单的单一数据源：下拉菜单与 Modal 左侧菜单共用，避免两处顺序不一致。
 * 菜单顺序：设置 / 意见反馈 / 捐赠 / —— / 检查更新 / 关于 / —— / 帮助。
 * 文案统一走 i18n（menu.<key> / page.<key>），增删语言不需改本文件。
 * 「意见反馈」与「帮助」是跳转动作（官方论坛），没有对应的内容页。
 */
import type { Component } from "vue";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  CircleHelp, Heart, Info, MessageSquare, RefreshCw, Settings,
} from "@lucide/vue";
import { t } from "../i18n/index";

export type MenuKey = "settings" | "feedback" | "donate" | "update" | "about" | "help";
export type PageKey = Exclude<MenuKey, "help">;

export interface MenuDef {
  key: MenuKey;
  icon: Component;
  action?: boolean; // true = 点击执行动作，不切换内容页
}

/** 下拉菜单按组渲染，每组之间插一条分隔线。 */
export const MENU_GROUPS: MenuDef[][] = [
  [
    { key: "settings", icon: Settings },
    { key: "feedback", icon: MessageSquare, action: true },
    { key: "donate", icon: Heart },
  ],
  [
    { key: "update", icon: RefreshCw },
    { key: "about", icon: Info },
  ],
  [
    { key: "help", icon: CircleHelp, action: true },
  ],
];

export const menuLabel = (key: MenuKey): string => t(`menu.${key}`);
export const pageTitle = (key: PageKey): string => t(`page.${key}`);

/** 官方论坛地址：「帮助」→ 交流板块；「意见反馈」→ 反馈板块。 */
export const FORUM_URL = "https://q.graent.cn/forum.php?mod=forumdisplay&fid=7";
export const FEEDBACK_URL = "https://q.graent.cn/forum.php?gid=3";

/** 跳转指定论坛地址；返回空串表示成功，否则返回给用户看的提示。 */
async function openForumUrl(url: string): Promise<string> {
  if (!url) return t("forum.notConfigured");
  try {
    await openUrl(url);
    return "";
  } catch {
    return t("forum.failed");
  }
}

/** 「帮助」跳转论坛交流板块。 */
export const openForum = (): Promise<string> => openForumUrl(FORUM_URL);
/** 「意见反馈」跳转论坛反馈板块。 */
export const openFeedback = (): Promise<string> => openForumUrl(FEEDBACK_URL);
