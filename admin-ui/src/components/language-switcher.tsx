import { LanguagesIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { MenuGroupLabel, MenuRadioGroup, MenuRadioItem } from '@/components/ui/menu'
import { useI18n, type Language } from '@/lib/i18n'

export function LanguageSwitcher({ compact = false }: { compact?: boolean }) {
  const { language, toggleLanguage } = useI18n()
  const switchingToEnglish = language === 'zh-CN'
  const label = switchingToEnglish ? '切换至英文界面' : 'Switch interface to Chinese'

  return (
    <Button
      type="button"
      size={compact ? 'icon-lg' : 'sm'}
      variant="outline"
      onClick={toggleLanguage}
      aria-label={label}
      title={label}
    >
      <LanguagesIcon />
      {!compact && <span>{switchingToEnglish ? 'EN' : '中文'}</span>}
    </Button>
  )
}

/** 中英文的显示名。写各自的母语，不做翻译——挑语言的人未必读得懂当前这一种。 */
const LANGUAGE_NAMES: Record<Language, string> = {
  'zh-CN': '中文',
  en: 'English',
}

/**
 * 窄屏那枚 ⋮ 里的语言选项。
 *
 * 不复用上面那枚切换按钮：两种语言时「切换」与「选择」看着一样，但按钮点下去才知道换到了
 * 哪一种，而菜单里两项并列、当前那项带对勾——头部按钮位省下来给主操作（添加账号）。
 */
export function LanguageMenuItems() {
  const { language, setLanguage, t } = useI18n()

  return (
    <>
      <MenuGroupLabel>{t('语言', 'Language')}</MenuGroupLabel>
      <MenuRadioGroup value={language}>
        {(Object.keys(LANGUAGE_NAMES) as Language[]).map((value) => (
          <MenuRadioItem key={value} value={value} onClick={() => setLanguage(value)}>
            {LANGUAGE_NAMES[value]}
          </MenuRadioItem>
        ))}
      </MenuRadioGroup>
    </>
  )
}
