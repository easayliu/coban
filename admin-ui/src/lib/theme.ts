/**
 * 深浅色主题：系统 / 浅色 / 深色三态。
 *
 * 只跟随系统在这类常驻页面上不够用——同一台机器上，白天跟着系统、夜里想单独把控制台
 * 压暗是常见诉求。选择写进 localStorage（与其他界面偏好同一 `coban.` 前缀），
 * 'system' 时继续监听 `prefers-color-scheme`，切到显式浅/深后系统变化不再干预。
 */

export const THEME_MODES = ['system', 'light', 'dark'] as const
export type ThemeMode = (typeof THEME_MODES)[number]

const KEY = 'coban.theme'

function query(): MediaQueryList {
  return window.matchMedia('(prefers-color-scheme: dark)')
}

/** localStorage 在隐私模式/禁用存储时读写会抛异常，一律降级为「跟随系统、不持久化」。 */
export function readThemeMode(): ThemeMode {
  try {
    const raw = localStorage.getItem(KEY)
    return (THEME_MODES as readonly string[]).includes(raw ?? '') ? (raw as ThemeMode) : 'system'
  } catch {
    return 'system'
  }
}

function resolveDark(mode: ThemeMode): boolean {
  return mode === 'system' ? query().matches : mode === 'dark'
}

/** 给 <html> 切换 .dark（配合 `@custom-variant dark`）。 */
export function applyThemeMode(mode: ThemeMode): void {
  document.documentElement.classList.toggle('dark', resolveDark(mode))
}

export function writeThemeMode(mode: ThemeMode): void {
  try {
    localStorage.setItem(KEY, mode)
  } catch {
    // 存不下就算了，不影响当次使用
  }
  applyThemeMode(mode)
}

/**
 * 在渲染前调用：先按存下来的选择上色，避免首帧闪一下另一套配色；
 * 同时挂上系统主题监听，'system' 模式下继续跟随。
 */
export function initTheme(): void {
  applyThemeMode(readThemeMode())
  query().addEventListener('change', () => {
    if (readThemeMode() === 'system') applyThemeMode('system')
  })
}
