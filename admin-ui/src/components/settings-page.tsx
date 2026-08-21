import { useEffect } from 'react'
import {
  ArrowLeftIcon,
  CableIcon,
  LockKeyholeIcon,
  SlidersHorizontalIcon,
} from 'lucide-react'
import {
  AccessSettingsContent,
  LimitsSettingsContent,
  SecuritySettingsContent,
} from '@/components/access-settings'
import { AppFooter } from '@/components/app-footer'
import { LanguageSwitcher } from '@/components/language-switcher'
import { ThemeSwitcher } from '@/components/theme-switcher'
import { LogoMark } from '@/components/logo-mark'
import { Button } from '@/components/ui/button'
import {
  Select,
  SelectItem,
  SelectPopup,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Tabs, TabsList, TabsPanel, TabsTab } from '@/components/ui/tabs'
import { useI18n } from '@/lib/i18n'
import { useMediaQuery } from '@/lib/use-media-query'

export type SettingsSection = 'access' | 'limits' | 'security'

export function SettingsPage({
  section,
  onSectionChange,
  onBack,
}: {
  section: SettingsSection
  onSectionChange: (section: SettingsSection) => void
  onBack: () => void
}) {
  const { t } = useI18n()
  const sections = [
    {
      key: 'access',
      label: t('客户端接入', 'Client access'),
      description: t(
        '管理接入地址、身份验证 Key 与 Codex 配置。',
        'Manage the endpoint, authentication key, and Codex setup.',
      ),
      navDescription: t('地址、Key 与配置片段', 'Endpoint, key, and setup'),
      icon: CableIcon,
    },
    {
      key: 'limits',
      label: t('调度与限流', 'Scheduling & limits'),
      description: t(
        '配置账号轮换的重试次数、每账号 RPM 上限，以及额度将满时的处置。',
        'Configure retry budget across accounts, per-account RPM limits, and what happens as quota fills up.',
      ),
      navDescription: t('重试、RPM 与额度阈值', 'Retries, RPM, and quota thresholds'),
      icon: SlidersHorizontalIcon,
    },
    {
      key: 'security',
      label: t('控制台安全', 'Console security'),
      description: t(
        '设置管理密码，保护系统设置与账号数据。',
        'Set an admin password to protect settings and account data.',
      ),
      navDescription: t('登录与管理密码', 'Sign-in and admin password'),
      icon: LockKeyholeIcon,
    },
  ] as const
  const active = sections.find((item) => item.key === section) ?? sections[0]
  const ActiveIcon = active.icon
  const desktopNavigation = useMediaQuery('(min-width: 64rem)')
  const selectItems = sections.map((item) => ({ label: item.label, value: item.key }))

  const changeSection = (value: string | null) => {
    if (value && sections.some((item) => item.key === value)) {
      onSectionChange(value as SettingsSection)
    }
  }

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${active.label} · Coban`
    return () => {
      document.title = previousTitle
    }
  }, [active.label])

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <header className="app-header sticky top-0 z-20 border-b bg-background/92 backdrop-blur-md">
        <div className="page-frame flex h-14 items-center justify-between gap-3 sm:h-16">
          <Button
            aria-label={t('返回账号页', 'Back to accounts')}
            className="-ml-2 h-auto min-w-0 justify-start gap-2.5 px-2 py-1.5 sm:gap-3"
            title={t('返回账号页', 'Back to accounts')}
            variant="ghost"
            onClick={onBack}
          >
            <span className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-lg">
              <LogoMark className="size-[1.125rem]" />
            </span>
            <span className="min-w-0 text-left">
              <span className="block text-sm font-semibold leading-none tracking-tight">Coban</span>
              <span className="mt-1 hidden whitespace-nowrap text-xs font-normal text-muted-foreground sm:block">
                Codex Gateway
              </span>
            </span>
          </Button>
          <div className="flex items-center gap-2">
            <LanguageSwitcher compact />
            <ThemeSwitcher compact />
            <Button
              aria-label={t('返回账号', 'Back to accounts')}
              className="max-sm:size-10 max-sm:px-0"
              size="sm"
              title={t('返回账号', 'Back to accounts')}
              variant="outline"
              onClick={onBack}
            >
              <ArrowLeftIcon aria-hidden="true" />
              <span className="max-sm:sr-only">{t('返回账号', 'Back to accounts')}</span>
            </Button>
          </div>
        </div>
      </header>

      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
        <div className="space-y-5 sm:space-y-7">
          <section aria-labelledby="settings-page-title" className="max-w-2xl">
            <h1
              className="min-w-0 text-xl font-semibold tracking-tight sm:text-2xl"
              id="settings-page-title"
            >
              {t('系统设置', 'System settings')}
            </h1>
            <p className="mt-1.5 text-sm leading-6 text-muted-foreground">
              {t(
                '集中管理 Coban 的客户端接入、账号调度与控制台安全。',
                'Manage Coban client access, account scheduling, and console security.',
              )}
            </p>
          </section>

          <Tabs
            className="min-w-0 gap-5 lg:items-start lg:gap-8"
            orientation={desktopNavigation ? 'vertical' : 'horizontal'}
            value={section}
            onValueChange={changeSection}
          >
            <div className="settings-tabs-bar sticky z-10 min-w-0 self-start bg-muted/95 py-2 backdrop-blur lg:top-24 lg:w-60 lg:shrink-0 lg:bg-transparent lg:py-0 lg:backdrop-blur-none">
              <div className="lg:hidden">
                <label className="sr-only" htmlFor="settings-section-select">
                  {t('设置分类', 'Settings category')}
                </label>
                <Select items={selectItems} value={section} onValueChange={changeSection}>
                  <SelectTrigger id="settings-section-select" aria-label={t('设置分类', 'Settings category')}>
                    <ActiveIcon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
                    <SelectValue />
                  </SelectTrigger>
                  <SelectPopup>
                    {sections.map((item) => {
                      const Icon = item.icon
                      return (
                        <SelectItem key={item.key} value={item.key}>
                          <span className="flex min-w-0 items-center gap-2">
                            <Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
                            <span className="truncate">{item.label}</span>
                          </span>
                        </SelectItem>
                      )
                    })}
                  </SelectPopup>
                </Select>
              </div>

              <TabsList
                aria-label={t('设置分类', 'Settings categories')}
                className="hidden w-full items-stretch rounded-xl p-1 lg:flex"
              >
                {sections.map((item) => {
                  const Icon = item.icon
                  return (
                    <TabsTab
                      className="h-auto min-h-15 min-w-0 grow-0 items-start whitespace-normal px-3 py-2.5 text-left"
                      key={item.key}
                      value={item.key}
                    >
                      <span className="mt-0.5 flex size-5 shrink-0 items-center justify-center">
                        <Icon aria-hidden="true" className="size-4" />
                      </span>
                      <span className="min-w-0 flex-1 text-left">
                        <span className="block font-medium">{item.label}</span>
                        <span className="mt-1 block max-w-full text-xs leading-4 text-muted-foreground">
                          {item.navDescription}
                        </span>
                      </span>
                    </TabsTab>
                  )
                })}
              </TabsList>
            </div>

            <div className="min-w-0 flex-1">
              <header className="mb-5 flex items-start gap-3">
                <span className="mt-0.5 flex size-9 shrink-0 items-center justify-center rounded-xl border bg-background shadow-xs/5">
                  <ActiveIcon aria-hidden="true" className="size-4 text-muted-foreground" />
                </span>
                <div className="min-w-0">
                  <h2 className="font-semibold text-lg tracking-tight">{active.label}</h2>
                  <p className="mt-1 max-w-2xl text-sm leading-5 text-muted-foreground">
                    {active.description}
                  </p>
                </div>
              </header>

              <TabsPanel className="min-w-0" value="access">
                {section === 'access' && <AccessSettingsContent />}
              </TabsPanel>
              <TabsPanel className="min-w-0" value="limits">
                {section === 'limits' && <LimitsSettingsContent />}
              </TabsPanel>
              <TabsPanel className="min-w-0" value="security">
                {section === 'security' && <SecuritySettingsContent />}
              </TabsPanel>
            </div>
          </Tabs>
        </div>
      </main>

      <AppFooter />
    </div>
  )
}
