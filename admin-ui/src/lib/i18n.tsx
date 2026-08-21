import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react'

export const LANGUAGES = ['zh-CN', 'en'] as const
export type Language = (typeof LANGUAGES)[number]

const STORAGE_KEY = 'coban.language'

export function parseLanguage(value: string | null | undefined): Language | null {
  if (value === 'zh-CN' || value === 'zh') return 'zh-CN'
  if (value === 'en-US' || value === 'en') return 'en'
  return null
}

function readLanguage(): Language | null {
  try {
    return parseLanguage(localStorage.getItem(STORAGE_KEY))
  } catch {
    // 存储不可用时保持原有中文默认，不影响页面使用。
  }
  return null
}

function persistLanguage(language: Language): void {
  try {
    localStorage.setItem(STORAGE_KEY, language)
  } catch {
    // 隐私模式或禁用存储时只保留本次会话状态。
  }
}

export function localize(language: Language, chinese: string, english: string): string {
  return language === 'zh-CN' ? chinese : english
}

interface I18nContextValue {
  language: Language
  locale: 'zh-CN' | 'en-US'
  setLanguage: (language: Language) => void
  toggleLanguage: () => void
  t: (chinese: string, english: string) => string
}

const I18nContext = createContext<I18nContextValue | null>(null)

export function LanguageProvider({
  children,
  initialLanguage,
  persist = true,
  onLanguageChange,
}: {
  children: ReactNode
  initialLanguage?: Language | null
  persist?: boolean
  onLanguageChange?: (language: Language) => void
}) {
  const [language, setLanguageState] = useState<Language>(
    () => initialLanguage ?? (persist ? readLanguage() : null) ?? 'zh-CN',
  )

  const setLanguage = useCallback((next: Language) => {
    setLanguageState(next)
    if (persist) persistLanguage(next)
    onLanguageChange?.(next)
  }, [onLanguageChange, persist])

  const toggleLanguage = useCallback(() => {
    setLanguage(language === 'zh-CN' ? 'en' : 'zh-CN')
  }, [language, setLanguage])

  const t = useCallback(
    (chinese: string, english: string) => localize(language, chinese, english),
    [language],
  )

  useEffect(() => {
    document.documentElement.lang = language
  }, [language])

  useEffect(() => {
    if (!persist) return undefined
    const syncAcrossTabs = (event: StorageEvent) => {
      const next = parseLanguage(event.newValue)
      if (event.key === STORAGE_KEY && next) {
        setLanguageState(next)
      }
    }
    window.addEventListener('storage', syncAcrossTabs)
    return () => window.removeEventListener('storage', syncAcrossTabs)
  }, [persist])

  const value = useMemo<I18nContextValue>(() => ({
    language,
    locale: language === 'zh-CN' ? 'zh-CN' : 'en-US',
    setLanguage,
    toggleLanguage,
    t,
  }), [language, setLanguage, t, toggleLanguage])

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>
}

export function useI18n(): I18nContextValue {
  const context = useContext(I18nContext)
  if (!context) throw new Error('useI18n must be used inside LanguageProvider')
  return context
}
