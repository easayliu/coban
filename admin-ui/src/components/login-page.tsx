import { useEffect, useState } from 'react'
import { useMutation } from '@tanstack/react-query'
import { ArrowRightIcon, EyeIcon, EyeOffIcon, LockKeyholeIcon } from 'lucide-react'
import { login } from '@/api/auth'
import { setPw } from '@/api/client'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardPanel, CardTitle } from '@/components/ui/card'
import { Field, FieldError, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from '@/components/ui/input-group'
import { LanguageSwitcher } from '@/components/language-switcher'
import { ThemeSwitcher } from '@/components/theme-switcher'
import { LogoMark } from '@/components/logo-mark'
import { useI18n } from '@/lib/i18n'

/** 管理登录页（已设置密码时展示）。登录成功回调 onSuccess(password)。 */
export function LoginPage({ onSuccess }: { onSuccess: (password: string) => void }) {
  const { t, language } = useI18n()
  const [password, setPassword] = useState('')
  const [show, setShow] = useState(false)

  useEffect(() => {
    const previousTitle = document.title
    document.title = t('管理登录 · Coban', 'Admin sign-in · Coban')
    return () => {
      document.title = previousTitle
    }
  }, [t])

  const doLogin = useMutation({
    mutationFn: () => login(password),
    onSuccess: () => {
      setPw(password)
      onSuccess(password)
    },
  })

  return (
    <div className="app-shell relative grid min-h-dvh place-items-center px-4 py-8 text-foreground sm:py-10">
      <div className="absolute end-4 top-4 flex items-center gap-2 sm:end-6 sm:top-6">
        <LanguageSwitcher />
        <ThemeSwitcher />
      </div>
      <div className="w-full max-w-sm">
        <div className="mb-6 flex flex-col items-center text-center">
          <div className="brand-mark flex size-12 items-center justify-center rounded-xl text-brand-foreground shadow-brand">
            <LogoMark className="size-7" />
          </div>
          <div className="mt-4 text-base font-semibold leading-none tracking-tight">Coban</div>
          <div className="mt-1.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Claude Code Gateway
          </div>
        </div>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base leading-tight">
              <LockKeyholeIcon aria-hidden="true" className="size-4 text-muted-foreground" />
              {t('管理登录', 'Admin sign-in')}
            </CardTitle>
            <CardDescription>
              {t('输入管理密码以继续访问控制台。', 'Enter the admin password to continue to the console.')}
            </CardDescription>
          </CardHeader>
          <CardPanel>
            <Form
              className="space-y-4"
              onSubmit={(event) => {
                event.preventDefault()
                if (password) doLogin.mutate()
              }}
            >
              <Field invalid={doLogin.isError}>
                <FieldLabel htmlFor="admin-password">
                  {t('管理密码', 'Admin password')}
                </FieldLabel>
                <InputGroup>
                  <InputGroupInput
                    id="admin-password"
                    autoFocus
                    aria-invalid={doLogin.isError || undefined}
                    onChange={(event) => setPassword(event.target.value)}
                    placeholder={t('请输入管理密码', 'Enter the admin password')}
                    type={show ? 'text' : 'password'}
                    value={password}
                  />
                  <InputGroupAddon align="inline-end">
                    <Button
                      aria-label={show ? t('隐藏密码', 'Hide password') : t('显示密码', 'Show password')}
                      size="icon-xs"
                      title={show ? t('隐藏密码', 'Hide password') : t('显示密码', 'Show password')}
                      type="button"
                      variant="ghost"
                      onClick={() => setShow((visible) => !visible)}
                    >
                      {show ? <EyeOffIcon /> : <EyeIcon />}
                    </Button>
                  </InputGroupAddon>
                </InputGroup>
                {doLogin.isError && <FieldError>{extractError(doLogin.error, language)}</FieldError>}
              </Field>
              <Button
                className="w-full"
                disabled={!password}
                loading={doLogin.isPending}
                type="submit"
              >
                <ArrowRightIcon aria-hidden="true" />
                {t('登录', 'Sign in')}
              </Button>
            </Form>
          </CardPanel>
        </Card>
      </div>
    </div>
  )
}
