import { MonitorIcon, MoonIcon, SunIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { MenuGroupLabel, MenuRadioGroup, MenuRadioItem } from '@/components/ui/menu'
import { useI18n } from '@/lib/i18n'
import { THEME_MODES, useThemeMode, type ThemeMode } from '@/lib/theme'

const ICONS: Record<ThemeMode, typeof MonitorIcon> = {
  system: MonitorIcon,
  light: SunIcon,
  dark: MoonIcon,
}

/** 三态的显示名。按钮的 aria-label 与菜单里那组单选共用，两处不能各叫一套。 */
function useThemeNames(): (value: ThemeMode) => string {
  const { t } = useI18n()
  return (value) => ({
    system: t('跟随系统', 'System'),
    light: t('浅色', 'Light'),
    dark: t('深色', 'Dark'),
  })[value]
}

/**
 * 系统 → 浅色 → 深色 循环。宽屏做成循环按钮而不是下拉：三态本来就少，
 * 头部按钮位紧张，多一个弹层不划算；当前模式由图标直接表达。
 *
 * 窄屏走的是 [`ThemeMenuItems`]——那边摊在菜单里，理由见它自己那条注。
 */
export function ThemeSwitcher({ compact = false }: { compact?: boolean }) {
  const { t } = useI18n()
  const [mode, setMode] = useThemeMode()
  const next = THEME_MODES[(THEME_MODES.indexOf(mode) + 1) % THEME_MODES.length]
  const Icon = ICONS[mode]
  const name = useThemeNames()
  const label = t(`外观：${name(mode)}，点击切换到${name(next)}`, `Appearance: ${name(mode)}. Switch to ${name(next)}`)

  return (
    <Button
      type="button"
      size={compact ? 'icon-lg' : 'icon-sm'}
      variant="outline"
      onClick={() => setMode(next)}
      aria-label={label}
      title={label}
    >
      <Icon />
    </Button>
  )
}

/**
 * 窄屏那枚 ⋮ 里的外观选项：三态摆成一组单选，当前值有对勾。
 *
 * 与上面那枚循环按钮**不是两套状态**（同走 [`useThemeMode`]）——两处同时挂在页面上（一个
 * `sm:hidden`、一个 `hidden sm:flex`），各存一份的话转屏之后对勾会停在旧值上。
 *
 * 手机上不复用循环按钮：循环控件要连点两次才到目标，而且点之前看不出下一个是什么；菜单
 * 展开后三个选项一次到位，也更符合触屏「摊开来点」的用法。
 */
export function ThemeMenuItems() {
  const { t } = useI18n()
  const [mode, setMode] = useThemeMode()
  const name = useThemeNames()

  return (
    <>
      <MenuGroupLabel>{t('外观', 'Appearance')}</MenuGroupLabel>
      <MenuRadioGroup value={mode}>
        {THEME_MODES.map((value) => {
          const Icon = ICONS[value]
          return (
            <MenuRadioItem key={value} value={value} onClick={() => setMode(value)}>
              <span className="flex items-center gap-2"><Icon />{name(value)}</span>
            </MenuRadioItem>
          )
        })}
      </MenuRadioGroup>
    </>
  )
}
