import { LanguagesIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useI18n } from '@/lib/i18n'

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
