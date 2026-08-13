import { createContext, type ReactNode, useContext, useEffect, useMemo, useState } from "react";

export type Locale = "zh-CN" | "en";

const messages = {
  "zh-CN": {
    appName: "into-markdown 控制台",
    skip: "跳到主要内容",
    status: "服务状态",
    language: "语言",
    theme: "主题",
    system: "跟随系统",
    light: "浅色",
    dark: "深色",
    loading: "正在检查本地服务…",
    retry: "重试",
    apiAvailable: "本地 API 可用",
    apiUnavailable: "本地 API 不可用",
    consoleUnavailable: "文档控制台功能尚不可用",
    unavailableDetail: "此页面只提供安全控制台壳。文档、任务与管理功能不在当前范围内。",
    errorTitle: "无法读取服务状态",
    errorDetail: "请确认 into-md ui 仍在运行，然后重试。",
    unexpectedTitle: "页面遇到问题",
    unexpectedDetail: "页面已安全停止。你可以重新加载控制台。",
    reload: "重新加载",
    notFound: "页面不存在",
    backStatus: "返回服务状态",
  },
  en: {
    appName: "into-markdown console",
    skip: "Skip to main content",
    status: "Service status",
    language: "Language",
    theme: "Theme",
    system: "System",
    light: "Light",
    dark: "Dark",
    loading: "Checking the local service…",
    retry: "Retry",
    apiAvailable: "Local API available",
    apiUnavailable: "Local API unavailable",
    consoleUnavailable: "Document console features are unavailable",
    unavailableDetail: "This page provides the secure console shell only. Documents, jobs, and administration are outside its scope.",
    errorTitle: "Could not read service status",
    errorDetail: "Make sure into-md ui is still running, then retry.",
    unexpectedTitle: "The page encountered a problem",
    unexpectedDetail: "The page stopped safely. You can reload the console.",
    reload: "Reload",
    notFound: "Page not found",
    backStatus: "Back to service status",
  },
} as const;

export type MessageKey = keyof (typeof messages)["en"];
interface I18nValue {
  locale: Locale;
  setLocale(locale: Locale): void;
  t(key: MessageKey): string;
}

const I18nContext = createContext<I18nValue | null>(null);

function initialLocale(): Locale {
  return navigator.languages.some((locale) => locale.toLowerCase().startsWith("zh")) ? "zh-CN" : "en";
}

export function I18nProvider({ children }: { children: ReactNode }) {
  const [locale, setLocale] = useState<Locale>(initialLocale);
  useEffect(() => {
    document.documentElement.lang = locale;
    document.documentElement.dir = "ltr";
  }, [locale]);
  const value = useMemo<I18nValue>(
    () => ({ locale, setLocale, t: (key) => messages[locale][key] }),
    [locale],
  );
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18nValue {
  const value = useContext(I18nContext);
  if (!value) throw new Error("i18n provider is unavailable");
  return value;
}
