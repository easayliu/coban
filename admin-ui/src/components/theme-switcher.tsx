import { useState } from 'react'
import { MonitorIcon, MoonIcon, SunIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useI18n } from '@/lib/i18n'
import { readThemeMode, writeThemeMode, THEME_MODES, type ThemeMode } from '@/lib/theme'

const ICONS: Record<ThemeMode, typeof MonitorIcon> = {
  system: MonitorIcon,
  light: SunIcon,
  dark: MoonIcon,
}

/**
 * 系统 → 浅色 → 深色 循环。做成循环按钮而不是下拉：三态本来就少，
 * 头部按钮位紧张，多一个弹层不划算；当前模式由图标直接表达。
 */
export function ThemeSwitcher({ compact = false }: { compact?: boolean }) {
  const { t } = useI18n()
  const [mode, setMode] = useState<ThemeMode>(readThemeMode)
  const next = THEME_MODES[(THEME_MODES.indexOf(mode) + 1) % THEME_MODES.length]
  const Icon = ICONS[mode]
  const name = (value: ThemeMode) => ({
    system: t('跟随系统', 'System'),
    light: t('浅色', 'Light'),
    dark: t('深色', 'Dark'),
  })[value]
  const label = t(`外观：${name(mode)}，点击切换到${name(next)}`, `Appearance: ${name(mode)}. Switch to ${name(next)}`)

  return (
    <Button
      type="button"
      size={compact ? 'icon-lg' : 'icon-sm'}
      variant="outline"
      onClick={() => {
        setMode(next)
        writeThemeMode(next)
      }}
      aria-label={label}
      title={label}
    >
      <Icon />
    </Button>
  )
}
