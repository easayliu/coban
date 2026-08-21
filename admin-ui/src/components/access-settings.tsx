import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  CableIcon, CheckIcon, CopyIcon, EyeIcon, EyeOffIcon, GaugeIcon, KeyRoundIcon,
  LockKeyholeIcon, RefreshCwIcon, ShieldAlertIcon, TimerResetIcon,
} from 'lucide-react'
import { changePassword, getAuthState } from '@/api/auth'
import { clearPw } from '@/api/client'
import {
  getSettings, setApiKey, setCooldownSecs, setDefaultRpmLimit, setQuotaPausePct,
  setRateLimitRetryMax, type Settings,
} from '@/api/settings'
import { useI18n } from '@/lib/i18n'
import { extractError } from '@/lib/utils'
import { SettingsGroup } from '@/components/settings-group'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import {
  NumberField, NumberFieldDecrement, NumberFieldGroup, NumberFieldIncrement, NumberFieldInput,
} from '@/components/ui/number-field'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'

/** 设置页各分节共用的读取。所有写接口都回整份设置，故成功后直接塞进缓存即可。 */
function useSettings() {
  const qc = useQueryClient()
  const query = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const put = (settings: Settings) => qc.setQueryData(['settings'], settings)
  return { ...query, put }
}

function useSaveToast() {
  const { t, language } = useI18n()
  return {
    ok: (title: string) => toastManager.add({ title, type: 'success' }),
    fail: (error: unknown) => toastManager.add({
      title: t('保存失败', 'Save failed'),
      description: extractError(error, language),
      type: 'error',
    }),
  }
}

/**
 * 一个「数字 + 保存」的设置项。
 *
 * 抽出来是因为这一页有四个同构的项，各写一遍的话「改了没保存就切走」「保存中禁用」
 * 这类细节必然在某一个上漏掉。
 */
function NumberSetting({
  label, description, value, min, max, onSave, pending,
}: {
  label: string
  description: string
  value: number
  min: number
  max: number
  onSave: (next: number) => void
  pending: boolean
}) {
  const { t } = useI18n()
  const [draft, setDraft] = useState(value)
  // 服务端那份变了就跟着走：别人改过、或自己刚保存成功，输入框都该显示最新值。
  useEffect(() => setDraft(value), [value])
  const dirty = draft !== value
  return (
    <Field>
      <FieldLabel>{label}</FieldLabel>
      <div className="flex items-center gap-2">
        <NumberField
          value={draft}
          min={min}
          max={max}
          step={1}
          onValueChange={(v) => setDraft(Math.min(max, Math.max(min, Math.floor(v ?? min))))}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement />
            <NumberFieldInput aria-label={label} />
            <NumberFieldIncrement />
          </NumberFieldGroup>
        </NumberField>
        <Button size="sm" disabled={!dirty || pending} onClick={() => onSave(draft)}>
          {pending && <Spinner />}
          {t('保存', 'Save')}
        </Button>
      </div>
      <FieldDescription>{description}</FieldDescription>
    </Field>
  )
}

// ---------- 客户端接入 ----------

export function AccessSettingsContent() {
  const { t } = useI18n()
  const { data, put } = useSettings()
  const toast = useSaveToast()
  const [draft, setDraft] = useState('')
  const [show, setShow] = useState(false)
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    setDraft(data?.api_key ?? '')
    setShow(false)
  }, [data?.api_key])

  const save = useMutation({
    mutationFn: (key: string) => setApiKey(key),
    onSuccess: (settings) => {
      put(settings)
      toast.ok(settings.api_key
        ? t('接入 Key 已保存', 'Access key saved')
        : t('接入 Key 已清除', 'Access key cleared'))
    },
    onError: toast.fail,
  })

  const envManaged = data?.env_managed ?? false
  const currentKey = data?.api_key ?? ''
  const baseUrl = window.location.origin

  /** 生成一把够长的随机 key。`crypto.getRandomValues` 而不是 `Math.random`——后者不是密码学随机源。 */
  const generate = () => {
    const bytes = new Uint8Array(24)
    crypto.getRandomValues(bytes)
    setDraft(`coban-${Array.from(bytes).map((b) => b.toString(16).padStart(2, '0')).join('')}`)
    setShow(true)
  }

  // 直接可粘进 ~/.codex/config.toml 的片段。base_url 用当前浏览器地址推——从别的机器打开
  // 这个页面时，写死 127.0.0.1 会给出一个在那台机器上根本连不通的地址。
  const snippet = [
    'model_provider = "coban"',
    '',
    '[model_providers.coban]',
    'name = "coban"',
    `base_url = "${baseUrl}/v1"`,
    'wire_api = "responses"',
    ...(currentKey ? ['env_key = "COBAN_API_KEY"'] : []),
  ].join('\n')
  const envLine = currentKey ? `export COBAN_API_KEY=${show ? currentKey : t('[已隐藏]', '[hidden]')}` : ''

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(currentKey ? `${snippet}\n\n# ${envLine}` : snippet)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch (e) {
      toast.fail(e)
    }
  }

  return (
    <div className="space-y-5">
      <SettingsGroup
        icon={KeyRoundIcon}
        title={t('接入 Key', 'Access key')}
        description={t(
          '客户端调用代理时必须带上的 Key。留空则不校验来访身份——仅在可信的本机网络下这么用。',
          'The key callers must present. Leave it empty to skip caller authentication — only do that on a trusted local network.',
        )}
      >
        {envManaged && (
          <Alert>
            <ShieldAlertIcon />
            <AlertTitle>{t('由环境变量接管', 'Managed by the environment')}</AlertTitle>
            <AlertDescription>
              {t(
                '接入 Key 由 --api-key / COBAN_API_KEY 指定，网页上不可修改。',
                'The access key comes from --api-key / COBAN_API_KEY and cannot be changed here.',
              )}
            </AlertDescription>
          </Alert>
        )}
        <Form
          onSubmit={(e) => { e.preventDefault(); save.mutate(draft.trim()) }}
          className="flex flex-col gap-2 sm:flex-row sm:items-center"
        >
          <Input
            value={draft}
            type={show ? 'text' : 'password'}
            disabled={envManaged}
            placeholder={t('留空表示不校验', 'Empty means no authentication')}
            onChange={(e) => setDraft(e.target.value)}
            aria-label={t('接入 Key', 'Access key')}
            className="font-mono"
          />
          <div className="flex items-center gap-2">
            <Button
              type="button"
              size="icon"
              variant="outline"
              onClick={() => setShow((v) => !v)}
              aria-label={show ? t('隐藏', 'Hide') : t('显示', 'Show')}
            >
              {show ? <EyeOffIcon /> : <EyeIcon />}
            </Button>
            <Button type="button" variant="outline" disabled={envManaged} onClick={generate}>
              <RefreshCwIcon />
              {t('生成', 'Generate')}
            </Button>
            <Button type="submit" disabled={envManaged || save.isPending || draft.trim() === currentKey}>
              {save.isPending && <Spinner />}
              {t('保存', 'Save')}
            </Button>
          </div>
        </Form>
        {!currentKey && !envManaged && (
          <Alert variant="error">
            <ShieldAlertIcon />
            <AlertTitle>{t('当前不校验来访身份', 'Callers are not authenticated')}</AlertTitle>
            <AlertDescription>
              {t(
                '任何能访问这个端口的人都可以用你的账号发请求。对外暴露前请务必设置 Key。',
                'Anyone who can reach this port can spend your accounts. Set a key before exposing it.',
              )}
            </AlertDescription>
          </Alert>
        )}
      </SettingsGroup>

      <SettingsGroup
        icon={CableIcon}
        title={t('Codex 配置', 'Codex setup')}
        description={t(
          '粘进 ~/.codex/config.toml，再把 Key 导出到环境变量即可。',
          'Paste into ~/.codex/config.toml, then export the key as an environment variable.',
        )}
      >
        <pre className="overflow-x-auto rounded-lg border bg-muted/40 p-3 font-mono text-xs leading-5">
          {snippet}
          {envLine && `\n\n# ${envLine}`}
        </pre>
        <Button size="sm" variant="outline" onClick={() => void copy()}>
          {copied ? <CheckIcon /> : <CopyIcon />}
          {copied ? t('已复制', 'Copied') : t('复制配置片段', 'Copy setup snippet')}
        </Button>
      </SettingsGroup>
    </div>
  )
}

// ---------- 调度与限流 ----------

export function LimitsSettingsContent() {
  const { t } = useI18n()
  const { data, put } = useSettings()
  const toast = useSaveToast()

  const mutate = (fn: (n: number) => Promise<Settings>, title: string) =>
    // eslint-disable-next-line react-hooks/rules-of-hooks
    useMutation({
      mutationFn: fn,
      onSuccess: (settings) => { put(settings); toast.ok(title) },
      onError: toast.fail,
    })

  const retry = mutate(setRateLimitRetryMax, t('已保存重试次数', 'Retry budget saved'))
  const rpm = mutate(setDefaultRpmLimit, t('已保存默认 RPM 上限', 'Default RPM limit saved'))
  const pause = mutate(setQuotaPausePct, t('已保存额度阈值', 'Quota threshold saved'))
  const cooldown = mutate(setCooldownSecs, t('已保存冷却时长', 'Cooldown saved'))

  if (!data) return <Spinner />

  return (
    <div className="space-y-5">
      <SettingsGroup
        icon={RefreshCwIcon}
        title={t('账号轮换', 'Account rotation')}
        description={t(
          '一条请求被上游拒掉之后，最多再换几个账号重试。',
          'How many other accounts a single request may fall back to after upstream rejects it.',
        )}
      >
        <NumberSetting
          label={t('最多换号重试次数', 'Maximum retries across accounts')}
          description={t(
            '0 表示不重试，把上游的 429 原样交回客户端。每次重试都要重发整个请求体，设得太大会让一条打不通的请求拖很久。',
            'Set 0 to pass the upstream 429 straight back. Each retry resends the whole request body, so a large value makes a doomed request hang for a long time.',
          )}
          value={data.rate_limit_retry_max}
          min={0}
          max={8}
          pending={retry.isPending}
          onSave={(n) => retry.mutate(n)}
        />
      </SettingsGroup>

      <SettingsGroup
        icon={GaugeIcon}
        title={t('默认 RPM 上限', 'Default RPM limit')}
        description={t(
          '每个账号每分钟最多转发多少条请求。单个账号可在其卡片里单独覆盖。',
          'How many requests per minute each account may forward. Individual accounts can override this.',
        )}
      >
        <NumberSetting
          label={t('每分钟最多请求数', 'Maximum requests per minute')}
          description={t(
            '0 表示不限。计数在服务端内存里，重启即清零。',
            'Set 0 for no limit. Counting lives in server memory and resets on restart.',
          )}
          value={data.default_rpm_limit}
          min={0}
          max={100000}
          pending={rpm.isPending}
          onSave={(n) => rpm.mutate(n)}
        />
      </SettingsGroup>

      <SettingsGroup
        icon={TimerResetIcon}
        title={t('额度与冷却', 'Quota and cooldown')}
        description={t(
          '账号额度将满、或刚被上游限流时的处置方式。',
          'What happens when an account is nearly out of quota, or was just rate limited upstream.',
        )}
      >
        <NumberSetting
          label={t('额度暂停阈值（%）', 'Quota pause threshold (%)')}
          description={t(
            '任一额度窗口用到这个百分比就暂停该账号到窗口重置。0 表示不暂停。留出余量是为了避免一条长请求跑到一半撞穿额度——那时上游直接掐断流，客户端拿到的是半截响应。',
            'Pause an account once any quota window reaches this percentage, until that window resets. Set 0 to never pause. The margin exists so a long streaming request does not hit the wall mid-flight — upstream simply cuts the stream and the caller gets a truncated response.',
          )}
          value={data.quota_pause_pct}
          min={0}
          max={100}
          pending={pause.isPending}
          onSave={(n) => pause.mutate(n)}
        />
        <NumberSetting
          label={t('限流冷却时长（秒）', 'Rate-limit cooldown (seconds)')}
          description={t(
            '撞上游 429 之后该账号退出选号多久。上游给了 retry-after 时以它为准，这个值是没给时的兜底。',
            'How long an account stays out of rotation after an upstream 429. When upstream sends retry-after that value wins; this is the fallback.',
          )}
          value={data.cooldown_secs}
          min={1}
          max={86400}
          pending={cooldown.isPending}
          onSave={(n) => cooldown.mutate(n)}
        />
      </SettingsGroup>
    </div>
  )
}

// ---------- 控制台安全 ----------

export function SecuritySettingsContent() {
  const { t } = useI18n()
  const qc = useQueryClient()
  const toast = useSaveToast()
  const authQuery = useQuery({ queryKey: ['auth-state'], queryFn: getAuthState })
  const [pw, setPwDraft] = useState('')
  const [confirm, setConfirm] = useState('')

  const envManaged = authQuery.data?.env_managed ?? false
  const configured = authQuery.data?.configured ?? false

  const change = useMutation({
    mutationFn: (password: string) => changePassword(password),
    onSuccess: (_r, password) => {
      setPwDraft('')
      setConfirm('')
      void qc.invalidateQueries({ queryKey: ['auth-state'] })
      toast.ok(password ? t('管理密码已更新', 'Admin password updated') : t('管理密码已清除', 'Admin password cleared'))
      // 密码换了，本地存的那把旧的必然对不上——留着只会让下一次请求 401 然后被强制登出，
      // 主动清掉再让用户重新登录更干净。
      if (password) clearPw()
    },
    onError: toast.fail,
  })

  const mismatch = pw !== '' && confirm !== '' && pw !== confirm
  const tooShort = pw !== '' && pw.trim().length < 4

  return (
    <div className="space-y-5">
      {!configured && !envManaged && (
        <Alert variant="error">
          <ShieldAlertIcon />
          <AlertTitle>{t('管理接口当前无需登录', 'The admin API is currently open')}</AlertTitle>
          <AlertDescription>
            {t(
              '任何能访问这个端口的人都能读写设置、查看账号，也包括接入 Key 本身。设一个密码。',
              'Anyone who can reach this port can read and change settings, view accounts, and read the access key itself. Set a password.',
            )}
          </AlertDescription>
        </Alert>
      )}

      <SettingsGroup
        icon={LockKeyholeIcon}
        title={t('管理密码', 'Admin password')}
        description={t(
          '设置后，所有管理接口都需要登录。转发代理不受影响。',
          'Once set, every admin API requires sign-in. The forwarding proxy is unaffected.',
        )}
      >
        {envManaged ? (
          <Alert>
            <ShieldAlertIcon />
            <AlertTitle>{t('由环境变量接管', 'Managed by the environment')}</AlertTitle>
            <AlertDescription>
              {t(
                '管理密码由 --admin-password / COBAN_ADMIN_PASSWORD 指定，网页上不可修改。',
                'The admin password comes from --admin-password / COBAN_ADMIN_PASSWORD and cannot be changed here.',
              )}
            </AlertDescription>
          </Alert>
        ) : (
          <Form
            onSubmit={(e) => { e.preventDefault(); if (!mismatch && !tooShort) change.mutate(pw.trim()) }}
            className="space-y-3"
          >
            <Field>
              <FieldLabel>{t('新密码', 'New password')}</FieldLabel>
              <Input
                type="password"
                value={pw}
                autoComplete="new-password"
                onChange={(e) => setPwDraft(e.target.value)}
              />
              <FieldDescription>
                {t('至少 4 位；留空并保存表示清除密码。', 'At least 4 characters. Save an empty value to clear the password.')}
              </FieldDescription>
            </Field>
            <Field>
              <FieldLabel>{t('确认密码', 'Confirm password')}</FieldLabel>
              <Input
                type="password"
                value={confirm}
                autoComplete="new-password"
                onChange={(e) => setConfirm(e.target.value)}
              />
              {mismatch && (
                <FieldDescription className="text-destructive">
                  {t('两次输入不一致', 'The two entries do not match')}
                </FieldDescription>
              )}
            </Field>
            <Button type="submit" disabled={change.isPending || mismatch || tooShort}>
              {change.isPending && <Spinner />}
              {pw.trim() === '' ? t('清除密码', 'Clear password') : t('保存密码', 'Save password')}
            </Button>
          </Form>
        )}
      </SettingsGroup>
    </div>
  )
}
