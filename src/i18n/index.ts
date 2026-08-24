import { useCallback } from "react";
import { create } from "zustand";
import { persist } from "zustand/middleware";
import zhCN, { MessageKey } from "./zh-CN";
import enUS from "./en-US";

export type Locale = "zh-CN" | "en-US";

const dictionaries: Record<Locale, Record<MessageKey, string>> = {
  "zh-CN": zhCN,
  "en-US": enUS,
};

/** 首次启动按浏览器/系统语言猜测默认语言（中文环境 → zh-CN，其余 → en-US） */
function detectLocale(): Locale {
  try {
    return navigator.language.toLowerCase().startsWith("zh") ? "zh-CN" : "en-US";
  } catch {
    return "zh-CN";
  }
}

export type MessageVars = Record<string, string | number>;

function format(template: string, vars?: MessageVars): string {
  if (!vars) return template;
  let out = template;
  for (const [k, v] of Object.entries(vars)) {
    out = out.split(`{${k}}`).join(String(v));
  }
  return out;
}

export function translate(locale: Locale, key: MessageKey, vars?: MessageVars): string {
  const dict = dictionaries[locale] ?? dictionaries["zh-CN"];
  return format(dict[key] ?? zhCN[key] ?? key, vars);
}

interface I18nState {
  locale: Locale;
  setLocale: (locale: Locale) => void;
  toggleLocale: () => void;
}

export const useI18nStore = create<I18nState>()(
  persist(
    (set, get) => ({
      locale: detectLocale(),
      setLocale: (locale) => {
        set({ locale });
        applyLocaleSideEffects(locale);
      },
      toggleLocale: () => {
        const next: Locale = get().locale === "zh-CN" ? "en-US" : "zh-CN";
        set({ locale: next });
        applyLocaleSideEffects(next);
      },
    }),
    { name: "sqlink.locale" }
  )
);

/** 同步 <html lang> 与窗口标题 */
function applyLocaleSideEffects(locale: Locale) {
  try {
    document.documentElement.lang = locale;
    document.title = translate(locale, "app.windowTitle");
  } catch {
    /* 非 DOM 环境下忽略 */
  }
}

// 模块加载即应用一次（刷新后恢复已保存的语言）
applyLocaleSideEffects(useI18nStore.getState().locale);

/** React 组件内使用：const t = useT(); t("tree.title") */
export function useT() {
  const locale = useI18nStore((s) => s.locale);
  return useCallback(
    (key: MessageKey, vars?: MessageVars) => translate(locale, key, vars),
    [locale]
  );
}

/** 非组件环境（zustand store、事件回调外）使用当前语言取文案 */
export function t(key: MessageKey, vars?: MessageVars): string {
  return translate(useI18nStore.getState().locale, key, vars);
}
