import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

export type Theme = "light" | "dark";

const STORAGE_KEY = "sqlconnect-theme";

function readInitialTheme(): Theme {
  try {
    return localStorage.getItem(STORAGE_KEY) === "dark" ? "dark" : "light";
  } catch {
    return "light";
  }
}

function applyTheme(theme: Theme) {
  document.documentElement.classList.toggle("dark", theme === "dark");
  // 同步原生窗口标题栏（Windows 沉浸式深色标题栏）
  invoke("set_window_theme", { theme }).catch(() => {});
}

// 模块加载即应用，避免首屏闪烁
const initialTheme = readInitialTheme();
applyTheme(initialTheme);

interface ThemeState {
  theme: Theme;
  toggle: () => void;
}

export const useThemeStore = create<ThemeState>((set, get) => ({
  theme: initialTheme,
  toggle: () => {
    const next: Theme = get().theme === "dark" ? "light" : "dark";
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      /* localStorage 不可用时仅本次会话生效 */
    }
    applyTheme(next);
    set({ theme: next });
  },
}));
