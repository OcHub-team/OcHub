/**
 * Locale registry for the OcHub docs site.
 *
 * Nimbus has no i18n of its own — `config.locale` is a single string that
 * only feeds `<html lang>` and metadata. Locales are therefore modelled as
 * entries in its `versions` manifest (see astro.config.ts), which is the
 * only mechanism that scopes navigation to a non-primary collection:
 * `buildStructuralTree()` ignores its `collection` argument unless the
 * slug is listed in `versions.others`. Without that, every locale renders
 * the English sidebar, breadcrumbs and prev/next.
 *
 * That borrowed machinery gets one thing wrong for translations, and it
 * is corrected rather than inherited: versioning assumes the pages are
 * duplicates and canonicalises them onto the current version. A Japanese
 * page is not a duplicate of the English one. `BaseLayout.astro`
 * withholds `entryId` from `NimbusHead` to suppress that canonical, and
 * `getLocaleAlternates` below supplies `hreflang` links instead.
 *
 * Locale codes match `crates/app/i18n/` so the docs and the app agree on
 * what languages exist.
 */

import { getCollection } from "astro:content";

export interface Locale {
  /** Internal key, also the URL prefix segment (empty for the default). */
  key: "en" | "ja" | "zh";
  /** BCP-47 tag for `<html lang>` and `hreflang`. */
  tag: string;
  /** Astro collection backing this locale. */
  collection: string;
  /** URL prefix, "" for the default locale (mounted at root). */
  prefix: string;
  /** Name shown in the locale picker, in its own language. */
  label: string;
  /**
   * Whether build-time OG cards can render this language's script.
   *
   * `public/fonts/Inter-Bold.ttf` is the only font astro-og-canvas loads,
   * and it has no CJK glyphs — generating cards for ja/zh with it yields
   * boxes. Those locales fall back to the site-wide social image until a
   * CJK font ships in `public/fonts/` and `_og-card-config.ts` lists it.
   */
  hasOgFont: boolean;
}

export const LOCALES: readonly Locale[] = [
  {
    key: "en",
    tag: "en",
    collection: "docs",
    prefix: "",
    label: "English",
    hasOgFont: true,
  },
  {
    key: "ja",
    tag: "ja",
    collection: "docs-ja",
    prefix: "/ja",
    label: "日本語",
    hasOgFont: false,
  },
  {
    key: "zh",
    tag: "zh-Hans",
    collection: "docs-zh",
    prefix: "/zh",
    label: "简体中文",
    hasOgFont: false,
  },
] as const;

export const DEFAULT_LOCALE = LOCALES[0];

/** Look up a locale by key. Falls back to the default. */
export function localeByKey(key: string): Locale {
  return LOCALES.find((l) => l.key === key) ?? DEFAULT_LOCALE;
}

/** Look up the locale backing an Astro collection. Falls back to the default. */
export function localeByCollection(collection: string | undefined): Locale {
  if (!collection) return DEFAULT_LOCALE;
  return LOCALES.find((l) => l.collection === collection) ?? DEFAULT_LOCALE;
}

/** Absolute site path for an entry in a given locale. */
export function localePath(locale: Locale, entryId: string): string {
  const slug = entryId.replace(/^\/+|\/+$/g, "");
  return slug ? `${locale.prefix}/${slug}` : `${locale.prefix}/`;
}

/**
 * Human label for a section slug that is really a locale.
 *
 * Because locales are declared through the `versions` manifest, Nimbus
 * hands back the version slug (`"ja"`, `"zh"`) as the section slug — which
 * matches `Locale.key`. Returns `null` for anything that isn't a locale,
 * so callers keep their own label for ordinary sections.
 */
export function localeSectionLabel(slug: string): string | null {
  const locale = LOCALES.find((l) => l.key === slug);
  return locale ? locale.label : null;
}

/**
 * Build the `hreflang` alternate set for one entry.
 *
 * Only locales that actually contain the entry are emitted — a half
 * translated site should not advertise alternates that 404. `x-default`
 * points at the default locale when it has the page.
 */
export async function getLocaleAlternates(
  entryId: string,
): Promise<{ tag: string; href: string }[]> {
  const alternates: { tag: string; href: string }[] = [];

  for (const locale of LOCALES) {
    let entries;
    try {
      entries = await getCollection(locale.collection as never);
    } catch {
      // Collection not registered (or empty) — skip rather than fail the build.
      continue;
    }
    const exists = entries.some((e: { id: string }) => e.id === entryId);
    if (!exists) continue;

    alternates.push({ tag: locale.tag, href: localePath(locale, entryId) });
    if (locale.key === DEFAULT_LOCALE.key) {
      alternates.push({ tag: "x-default", href: localePath(locale, entryId) });
    }
  }

  return alternates;
}

/**
 * Chrome strings that live outside MDX content. Anything a reader sees
 * that isn't authored in a content file belongs here.
 *
 * English is declared separately so its keys define the required set:
 * the `satisfies` below then makes a missing translation a build error
 * rather than an `undefined` that renders as a blank label.
 */
const EN_STRINGS = {
  skipToContent: "Skip to content",
  language: "Language",
  closeSidebar: "Close sidebar",
  openNavigation: "Open navigation",
  tagline:
    "A native desktop control center for AI coding tools. Switch model providers, manage shared capabilities, and run a local gateway from one place.",
  overviewTitle: "Overview",
  overviewBlurb: "What OcHub manages",
  installTitle: "Install",
  installBlurb: "Homebrew and direct downloads",
  quickStartTitle: "Quick start",
  quickStartBlurb: "Make your first connection",
  buildTitle: "Build from source",
  buildBlurb: "Toolchain and dev commands",
  home: "Home",
  breadcrumb: "Breadcrumb",
  search: "Search",
  searchDocs: "Search documentation",
  filter: "Filter...",
  filterNav: "Filter navigation",
  tableOfContents: "Table of contents",
  onThisPage: "On this page",
  jumpToSection: "Jump to section",
  sectionOverview: "Overview",
  copyPage: "Copy page",
  copied: "Copied!",
  copyError: "Couldn't copy",
  viewAsMarkdown: "View as Markdown",
  updated: "Updated",
  previous: "Previous",
  next: "Next",
  sponsorsTitle: "Sponsors",
  sponsorsBlurb: "Relay providers that support OcHub",
} as const;

export const UI_STRINGS = {
  en: EN_STRINGS,
  ja: {
    skipToContent: "本文へスキップ",
    language: "言語",
    closeSidebar: "サイドバーを閉じる",
    openNavigation: "ナビゲーションを開く",
    tagline:
      "AI コーディングツールのためのネイティブデスクトップ管理センター。モデルプロバイダーの切り替え、共有機能の管理、ローカルゲートウェイの実行を一か所で。",
    overviewTitle: "概要",
    overviewBlurb: "OcHub が管理するもの",
    installTitle: "インストール",
    installBlurb: "Homebrew と直接ダウンロード",
    quickStartTitle: "クイックスタート",
    quickStartBlurb: "最初の接続を設定",
    buildTitle: "ソースからビルド",
    buildBlurb: "ツールチェーンと開発コマンド",
    home: "ホーム",
    breadcrumb: "パンくずリスト",
    search: "検索",
    searchDocs: "ドキュメントを検索",
    filter: "絞り込み...",
    filterNav: "ナビゲーションを絞り込む",
    tableOfContents: "目次",
    onThisPage: "このページの内容",
    jumpToSection: "セクションへ移動",
    sectionOverview: "概要",
    copyPage: "ページをコピー",
    copied: "コピーしました",
    copyError: "コピーできません",
    viewAsMarkdown: "Markdown で表示",
    updated: "更新日",
    previous: "前へ",
    next: "次へ",
    sponsorsTitle: "スポンサー",
    sponsorsBlurb: "OcHub を支援する中継プロバイダー",
  },
  zh: {
    skipToContent: "跳到正文",
    language: "语言",
    closeSidebar: "关闭侧边栏",
    openNavigation: "打开导航",
    tagline:
      "面向 AI 编程工具的原生桌面控制中心。切换模型供应商、管理共享能力、运行本地网关，都在一个地方完成。",
    overviewTitle: "概述",
    overviewBlurb: "OcHub 管理什么",
    installTitle: "安装",
    installBlurb: "Homebrew 与直接下载",
    quickStartTitle: "快速上手",
    quickStartBlurb: "完成第一次连接",
    buildTitle: "从源码构建",
    buildBlurb: "工具链与开发命令",
    home: "首页",
    breadcrumb: "面包屑导航",
    search: "搜索",
    searchDocs: "搜索文档",
    filter: "筛选...",
    filterNav: "筛选导航",
    tableOfContents: "目录",
    onThisPage: "本页内容",
    jumpToSection: "跳转到章节",
    sectionOverview: "概述",
    copyPage: "复制页面",
    copied: "已复制",
    copyError: "复制失败",
    viewAsMarkdown: "查看 Markdown",
    updated: "更新于",
    previous: "上一页",
    next: "下一页",
    sponsorsTitle: "赞助商",
    sponsorsBlurb: "支持 OcHub 的中转服务商",
  },
} as const satisfies Record<Locale["key"], Record<keyof typeof EN_STRINGS, string>>;

export function t(locale: Locale, key: keyof typeof EN_STRINGS): string {
  return UI_STRINGS[locale.key][key];
}
