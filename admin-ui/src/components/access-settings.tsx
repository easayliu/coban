import { useEffect, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  CableIcon, CheckIcon, CopyIcon, EyeIcon, EyeOffIcon, FingerprintIcon, GaugeIcon,
  LockKeyholeIcon, PinIcon, RefreshCwIcon, SaveIcon, ShieldAlertIcon, SparklesIcon, TimerResetIcon,
  Trash2Icon,
} from 'lucide-react'
import { changePassword, getAuthState, setup as setupPassword } from '@/api/auth'
import { clearPw, setPw } from '@/api/client'
import {
  getSettings, setApiKey, setCooldownSecs, setDefaultRpmLimit, setNormalizeToolOrder,
  setQuotaPausePct, setRateLimitRetryMax, setRateLimitRotate, setRateLimitWaitRetryMax,
  setRateLimitWaitSecs, setSessionLeaseSecs, setUpstreamUaMode, type Settings,
} from '@/api/settings'
import { useI18n } from '@/lib/i18n'
import { copyText, extractError } from '@/lib/utils'
import {
  AlertDialog,
  AlertDialogClose,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogPopup,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button, type ButtonProps } from '@/components/ui/button'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from '@/components/ui/input-group'
import { Input } from '@/components/ui/input'
import {
  NumberField, NumberFieldDecrement, NumberFieldGroup, NumberFieldIncrement, NumberFieldInput,
} from '@/components/ui/number-field'
import { Spinner } from '@/components/ui/spinner'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Switch } from '@/components/ui/switch'
import { toastManager } from '@/components/ui/toast'
import { SettingsGroup } from '@/components/settings-group'

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

/** Luban 设置页使用的小型复制按钮：优先走安全上下文 API，局域网 HTTP 也能回退复制。 */
function CopyButton({
  text,
  label,
  copiedLabel,
  size = 'icon-xs',
}: {
  text: string
  label: string
  copiedLabel: string
  size?: ButtonProps['size']
}) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  const timerRef = useRef<number | null>(null)

  useEffect(() => () => {
    if (timerRef.current !== null) window.clearTimeout(timerRef.current)
  }, [])

  return (
    <>
      <Button
        type="button"
        aria-label={copied ? copiedLabel : label}
        className={copied ? 'text-success' : undefined}
        size={size}
        title={copied ? copiedLabel : label}
        variant="ghost"
        onClick={async () => {
          if (!text) return
          if (!(await copyText(text))) {
            toastManager.add({
              title: t('复制失败', 'Copy failed'),
              description: t('请手动选择并复制内容。', 'Select and copy the content manually.'),
              type: 'error',
            })
            return
          }
          setCopied(true)
          if (timerRef.current !== null) window.clearTimeout(timerRef.current)
          timerRef.current = window.setTimeout(() => {
            setCopied(false)
            timerRef.current = null
          }, 1400)
        }}
      >
        {copied ? <CheckIcon /> : <CopyIcon />}
      </Button>
      <span className="sr-only" aria-live="polite">{copied ? copiedLabel : ''}</span>
    </>
  )
}

/** 设置项共用的保存 mutation，保持每个控件的 loading、错误提示和缓存更新口径一致。 */
function useSettingMutation<V extends number | boolean>(
  mutationFn: (value: V) => Promise<Settings>,
  successTitle: string,
) {
  const qc = useQueryClient()
  const toast = useSaveToast()
  return useMutation({
    mutationFn,
    onSuccess: (settings) => {
      qc.setQueryData(['settings'], settings)
      toast.ok(successTitle)
    },
    onError: toast.fail,
  })
}

/**
 * 一个「数字 + 保存」的设置项。
 *
 * 抽出来是因为这一页有四个同构的项，各写一遍的话「改了没保存就切走」「保存中禁用」
 * 这类细节必然在某一个上漏掉。
 */
/**
 * 一个开关项。与 [`NumberSetting`] 的区别是**拨完立刻保存**，没有「保存」按钮：数字要给人
 * 改完再确认的机会（半个数字是错的），而开关的中间态不存在，多一次点击只是多一次点击。
 */
function SwitchSetting({
  label, description, checked, onToggle, pending,
}: {
  label: string
  description: string
  checked: boolean
  onToggle: (next: boolean) => void
  pending: boolean
}) {
  return (
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{label}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">{description}</FieldDescription>
      </div>
      <div className="flex items-center gap-2 sm:justify-end">
        {pending && <Spinner />}
        <Switch checked={checked} disabled={pending} onCheckedChange={onToggle} aria-label={label} />
      </div>
    </Field>
  )
}

/**
 * 一个「几选一」的设置项。与 [`SwitchSetting`] 一样**选完立刻保存**：选项本身就是终态，
 * 没有「半个选择」这种中间态。
 */
function ChoiceSetting({
  label, description, value, options, onSelect, pending,
}: {
  label: string
  description: string
  value: number
  options: { value: number, label: string }[]
  onSelect: (next: number) => void
  pending: boolean
}) {
  // Select 的 value 走字符串：数字 0 在这套组件里与「没选」难分。
  const items = options.map((o) => ({ label: o.label, value: String(o.value) }))
  return (
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{label}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">{description}</FieldDescription>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto sm:justify-end">
        {pending && <Spinner />}
        <Select
          items={items}
          value={String(value)}
          onValueChange={(next) => { if (next !== null && Number(next) !== value) onSelect(Number(next)) }}
        >
          <SelectTrigger aria-label={label} className="w-full sm:w-64" disabled={pending}>
            <SelectValue />
          </SelectTrigger>
          <SelectPopup>
            {items.map((item) => (
              <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
            ))}
          </SelectPopup>
        </Select>
      </div>
    </Field>
  )
}

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
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{label}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">{description}</FieldDescription>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <NumberField
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
          value={draft}
          min={min}
          max={max}
          step={1}
          onValueChange={(v) => setDraft(Math.min(max, Math.max(min, Math.floor(v ?? min))))}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement aria-label={t('减少', 'Decrease')} />
            <NumberFieldInput aria-label={label} />
            <NumberFieldIncrement aria-label={t('增加', 'Increase')} />
          </NumberFieldGroup>
        </NumberField>
        <Button size="sm" disabled={!dirty || pending} onClick={() => onSave(draft)}>
          {pending && <Spinner />}
          {t('保存', 'Save')}
        </Button>
      </div>
    </Field>
  )
}

// ---------- 客户端接入 ----------

export function AccessSettingsContent() {
  const { t } = useI18n()
  const settingsQuery = useSettings()
  const { data, put } = settingsQuery
  const toast = useSaveToast()
  const [draft, setDraft] = useState('')
  const [show, setShow] = useState(false)
  const [revealedSnippetKey, setRevealedSnippetKey] = useState(false)
  const [clearKeyOpen, setClearKeyOpen] = useState(false)

  useEffect(() => {
    setDraft(data?.api_key ?? '')
    setShow(false)
    setRevealedSnippetKey(false)
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
    const hex = Array.from(bytes).map((byte) => byte.toString(16).padStart(2, '0')).join('')
    setDraft(`coban-${hex}`)
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
  // 单引号包住用户自填的 key；其中若真含单引号，用 POSIX shell 的标准拼接写法转义。
  const shellKey = currentKey ? `'${currentKey.split("'").join(`'"'"'`)}'` : ''
  const envCommand = currentKey ? `export COBAN_API_KEY=${shellKey}` : ''
  const visibleEnvCommand = currentKey && !revealedSnippetKey
    ? `export COBAN_API_KEY=${t('[已隐藏]', '[hidden]')}`
    : envCommand

  if (settingsQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner className="size-4" />
        {t('正在加载设置', 'Loading settings')}
      </div>
    )
  }

  if (settingsQuery.isError || !data) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">
          {t('无法读取当前设置', 'Unable to load the current settings')}
        </p>
        <Button
          size="sm"
          variant="outline"
          loading={settingsQuery.isFetching}
          onClick={() => settingsQuery.refetch()}
        >
          {t('重试', 'Retry')}
        </Button>
      </div>
    )
  }

  return (
    <>
      <div className="space-y-5">
        <SettingsGroup
          icon={CableIcon}
          title={t('连接与认证', 'Connection & authentication')}
          description={t(
            '复制 Codex 接入地址，并配置代理用于验证来访请求的 Key。',
            'Copy the Codex endpoint and configure the key used to authenticate incoming requests.',
          )}
        >
        <Field className="p-5">
          <FieldLabel>
            {t('接入地址', 'Access URL')}
            <code className="font-mono text-xs font-normal text-muted-foreground">base_url</code>
          </FieldLabel>
          <InputGroup>
            <InputGroupInput aria-label={t('接入地址', 'Access URL')} readOnly value={`${baseUrl}/v1`} />
            <InputGroupAddon align="inline-end">
              <CopyButton
                text={`${baseUrl}/v1`}
                label={t('复制接入地址', 'Copy access URL')}
                copiedLabel={t('已复制接入地址', 'Access URL copied')}
              />
            </InputGroupAddon>
          </InputGroup>
        </Field>

        <Field className="p-5">
          <FieldLabel>
            {t('接入 Key', 'Access key')}
            <code className="font-mono text-xs font-normal text-muted-foreground">COBAN_API_KEY</code>
          </FieldLabel>
          <InputGroup>
            <InputGroupInput
              aria-label={t('接入 Key', 'Access key')}
              onChange={(event) => setDraft(event.target.value)}
              placeholder={envManaged ? '' : t('留空则不校验来访', 'Leave blank to disable caller authentication')}
              readOnly={envManaged}
              type={show ? 'text' : 'password'}
              value={draft}
            />
            <InputGroupAddon className="gap-2" align="inline-end">
              <Button
                aria-label={show ? t('隐藏接入 Key', 'Hide access key') : t('显示接入 Key', 'Show access key')}
                size="icon-sm"
                title={show ? t('隐藏 Key', 'Hide key') : t('显示 Key', 'Show key')}
                variant="ghost"
                onClick={() => setShow((visible) => !visible)}
              >
                {show ? <EyeOffIcon /> : <EyeIcon />}
              </Button>
              <CopyButton
                text={draft}
                label={t('复制接入 Key', 'Copy access key')}
                copiedLabel={t('已复制接入 Key', 'Access key copied')}
                size="icon-sm"
              />
            </InputGroupAddon>
          </InputGroup>
          {!envManaged && (
            <div className="flex flex-wrap items-center gap-2">
              <Button size="sm" variant="outline" onClick={generate}>
                <SparklesIcon />
                {t('生成', 'Generate')}
              </Button>
              <Button
                size="sm"
                loading={save.isPending}
                disabled={draft.trim() === currentKey}
                onClick={() => save.mutate(draft.trim())}
              >
                <SaveIcon />
                {t('保存', 'Save')}
              </Button>
              {currentKey && (
                <Button
                  size="sm"
                  variant="destructive-outline"
                  disabled={save.isPending}
                  onClick={() => setClearKeyOpen(true)}
                >
                  <Trash2Icon />
                  {t('清空', 'Clear')}
                </Button>
              )}
            </div>
          )}
          {envManaged && (
            <FieldDescription>
              {t('由 --api-key / COBAN_API_KEY 接管，网页只读。', 'Managed by --api-key / COBAN_API_KEY; this page is read-only.')}
            </FieldDescription>
          )}
        </Field>

        <Field className="p-5">
          <div className="flex w-full min-w-0 items-center justify-between gap-2">
            <FieldLabel>{t('Codex 配置片段', 'Codex configuration')}</FieldLabel>
            <div className="flex shrink-0 items-center gap-2">
              <CopyButton
                text={snippet}
                label={t('复制 Codex 配置', 'Copy Codex configuration')}
                copiedLabel={t('已复制 Codex 配置', 'Codex configuration copied')}
                size="icon"
              />
            </div>
          </div>
          <pre className="max-w-full overflow-x-auto rounded-lg border bg-muted/72 p-3 font-mono text-xs leading-5">
            {snippet}
          </pre>
          <FieldDescription>
            {t('粘贴到 ~/.codex/config.toml。', 'Paste this into ~/.codex/config.toml.')}
          </FieldDescription>
        </Field>
        {currentKey && (
          <Field className="p-5">
            <div className="flex w-full min-w-0 items-center justify-between gap-2">
              <FieldLabel>{t('Key 环境变量', 'Key environment variable')}</FieldLabel>
              <div className="flex shrink-0 items-center gap-2">
                <Button
                  type="button"
                  aria-label={revealedSnippetKey ? t('隐藏环境变量中的 Key', 'Hide the key in the environment command') : t('显示环境变量中的 Key', 'Show the key in the environment command')}
                  size="icon"
                  title={revealedSnippetKey ? t('隐藏 Key', 'Hide key') : t('显示 Key', 'Show key')}
                  variant="ghost"
                  onClick={() => setRevealedSnippetKey((revealed) => !revealed)}
                >
                  {revealedSnippetKey ? <EyeOffIcon /> : <EyeIcon />}
                </Button>
                <CopyButton
                  text={envCommand}
                  label={t('复制完整环境变量（含 Key）', 'Copy the full environment command (includes key)')}
                  copiedLabel={t('已复制环境变量', 'Environment command copied')}
                  size="icon"
                />
              </div>
            </div>
            <pre className="max-w-full overflow-x-auto rounded-lg border bg-muted/72 p-3 font-mono text-xs leading-5">
              {visibleEnvCommand}
            </pre>
            <FieldDescription>
              {t('在启动 Codex 的终端里执行。为避免截图或录屏泄露，Key 默认隐藏。', 'Run this in the terminal that starts Codex. The key stays hidden by default to avoid capture in screenshots or recordings.')}
            </FieldDescription>
          </Field>
        )}
        {!currentKey && !envManaged && (
          <Alert className="mx-5 mb-5" variant="error">
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
      </div>
      <AlertDialog open={clearKeyOpen} onOpenChange={(nextOpen) => { if (!save.isPending) setClearKeyOpen(nextOpen) }}>
        <AlertDialogPopup className="sm:max-w-md">
          <AlertDialogHeader>
            <AlertDialogTitle>{t('清除接入 Key', 'Clear access key')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t('清除后，代理将不再校验客户端身份。', 'After clearing it, the proxy will no longer authenticate callers.')}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={save.isPending} variant="ghost" />}>
              {t('取消', 'Cancel')}
            </AlertDialogClose>
            <Button loading={save.isPending} variant="destructive" onClick={() => save.mutate('')}>
              {t('确认清除', 'Clear key')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </>
  )
}

// ---------- 调度与限流 ----------

export function LimitsSettingsContent() {
  const { t } = useI18n()
  const settingsQuery = useSettings()
  const { data } = settingsQuery
  const retry = useSettingMutation(setRateLimitRetryMax, t('已保存重试次数', 'Retry budget saved'))
  const rotate = useSettingMutation(
    setRateLimitRotate,
    t('已保存换号设置', 'Rotation setting saved'),
  )
  const waitSecs = useSettingMutation(
    setRateLimitWaitSecs,
    t('已保存等待上限', 'Wait ceiling saved'),
  )
  const waitRetry = useSettingMutation(
    setRateLimitWaitRetryMax,
    t('已保存同号重试次数', 'Same-account retry budget saved'),
  )
  const rpm = useSettingMutation(setDefaultRpmLimit, t('已保存默认 RPM 上限', 'Default RPM limit saved'))
  const pause = useSettingMutation(setQuotaPausePct, t('已保存额度阈值', 'Quota threshold saved'))
  const cooldown = useSettingMutation(setCooldownSecs, t('已保存冷却时长', 'Cooldown saved'))
  const lease = useSettingMutation(setSessionLeaseSecs, t('已保存租约时长', 'Lease duration saved'))
  const toolOrder = useSettingMutation(
    setNormalizeToolOrder,
    t('已保存工具顺序设置', 'Tool order setting saved'),
  )
  const uaMode = useSettingMutation(setUpstreamUaMode, t('已保存 UA 设置', 'UA setting saved'))

  if (settingsQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner className="size-4" />
        {t('正在加载调度设置', 'Loading scheduling settings')}
      </div>
    )
  }

  if (settingsQuery.isError || !data) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">
          {t('无法读取调度设置', 'Unable to load scheduling settings')}
        </p>
        <Button size="sm" variant="outline" loading={settingsQuery.isFetching} onClick={() => settingsQuery.refetch()}>
          {t('重试', 'Retry')}
        </Button>
      </div>
    )
  }

  return (
    <div className="space-y-5">
      <SettingsGroup
        icon={RefreshCwIcon}
        title={t('账号轮换', 'Account rotation')}
        description={t(
          '一条请求被上游拒掉之后，是换个账号重试，还是在原账号上等一等。',
          'After upstream rejects a request: fall back to another account, or wait it out on the current one.',
        )}
      >
        <SwitchSetting
          label={t('撞限流就换账号', 'Switch accounts on a rate limit')}
          description={t(
            '默认开：撞上游 429 时把这个号打上冷却、当场换下一个号——一堆号挂在这儿的意义就在这里。关掉之后这条请求只认选中的那个号：撞 429 就在原地等一等再发一遍，等待与次数见下面两项；次数用完仍是 429 就把上游的判决原样交回客户端，不再换号。号少、或者更在乎会话别乱跑时值得关——上游的提示缓存按账号存，换一次号等于把整段前缀重算一遍，代价可能比等几秒还大。',
            'On by default: an upstream 429 cools that account down and the request moves to the next one — that is the whole point of pooling accounts. Turn it off and a request sticks to the account it picked: on a 429 it waits in place and retries, bounded by the two settings below; once those run out the upstream 429 goes straight back to the client and no other account is tried. Worth turning off with few accounts, or when conversation stickiness matters — upstream caches prompts per account, so one switch re-reads the whole prefix, which can cost more than waiting a few seconds.',
          )}
          checked={data.rate_limit_rotate}
          pending={rotate.isPending}
          onToggle={(on) => rotate.mutate(on)}
        />
        {!data.rate_limit_rotate && (
          <>
            <NumberSetting
              label={t('同号重试最多等多久（秒）', 'Maximum wait before retrying (seconds)')}
              description={t(
                '等多久由上游说了算：优先按它给的 retry-after 或错误体里的恢复提示，都没有才按上面的限流冷却时长。这个值是上限——上游说要等的比它还长（额度用尽那种 429 常常是几小时），就当场把 429 交回客户端，挂着等只会让客户端自己先超时，而且什么也没等到。',
                'How long to wait is decided upstream: its retry-after, else the reset hint in the error body, else the rate-limit cooldown above. This value is a ceiling — when upstream asks for longer than this (a quota-exhausted 429 is often hours away), the 429 goes back to the client immediately, because hanging on would just hit the client\u2019s own timeout and gain nothing.',
              )}
              value={data.rate_limit_wait_secs}
              min={1}
              max={3600}
              pending={waitSecs.isPending}
              onSave={(n) => waitSecs.mutate(n)}
            />
            <NumberSetting
              label={t('同号最多重试次数', 'Maximum retries on the same account')}
              description={t(
                '在同一个账号上最多等着重发几次。0 表示一次都不等，撞 429 直接把上游的判决交回客户端。',
                'How many times a request may wait and resend on the same account. Set 0 to never wait and hand the upstream 429 straight back.',
              )}
              value={data.rate_limit_wait_retry_max}
              min={0}
              max={8}
              pending={waitRetry.isPending}
              onSave={(n) => waitRetry.mutate(n)}
            />
          </>
        )}
        <NumberSetting
          label={t('最多换号重试次数', 'Maximum retries across accounts')}
          description={t(
            '撞限流（429）时不受这个数字限制（上面那个开关管它）：那个号会被打上冷却，请求一路换到找出可用的号为止。这里管的是「连不上上游」那类故障——它慢、又要重发整个请求体，设得太大会让一条打不通的请求拖很久。0 表示完全不换号，把上游的判决（含 429）原样交回客户端。',
            'Rate limits (429) are not bounded by this number — the switch above governs those: the account is cooled down and the request keeps rotating until it finds a usable one. This bounds unreachable-upstream failures instead — those are slow and resend the whole request body, so a large value makes a doomed request hang for a long time. Set 0 to disable rotation entirely and pass the upstream verdict (429 included) straight back.',
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

      <SettingsGroup
        icon={PinIcon}
        title={t('会话粘性', 'Session stickiness')}
        description={t(
          '同一段对话尽量一直落在同一个账号上——上游的提示缓存按账号存，换号就等于把整段前缀重新算一遍。',
          'Keep one conversation on one account. Upstream caches prompts per account, so switching means re-reading the whole prefix.',
        )}
      >
        <NumberSetting
          label={t('落点租约时长（秒）', 'Placement lease (seconds)')}
          description={t(
            '一个账号真的服务过某段对话之后，这条对话在这么久之内还会优先回到它身上；每次请求都会续期，所以只有停下来不说话才会过期。有了它，加号删号不会打乱正在跑的对话。0 表示关闭，每次按会话内容现算落点——那时账号增删会让约 1/N 的对话换号。注意：租约压过优先级，改优先级只影响新对话；要立刻把流量从某个账号挪走请停用它。',
            'Once an account has actually served a conversation, that conversation returns to it for this long. Every request renews the lease, so it only expires after the conversation goes quiet. With it, adding or removing accounts leaves running conversations alone. Set 0 to disable and recompute placement from the conversation each time — then adding or removing an account remaps about 1/N of conversations. Note that a lease outranks priority, so priority changes only affect new conversations; disable an account to move traffic off it right away.',
          )}
          value={data.session_lease_secs}
          min={0}
          max={86400}
          pending={lease.isPending}
          onSave={(n) => lease.mutate(n)}
        />
        <SwitchSetting
          label={t('转发前把工具列表排序', 'Sort the tool list before forwarding')}
          description={t(
            '工具定义连同顺序都算在提示缓存的前缀里。有的客户端每轮发来的工具顺序都不一样，那就等于每轮都从零开始算——开着它能把这种客户端强行稳住。默认关：官方 codex CLI 的顺序本来就是固定的，那时排序一分不赚，还会让发上去的顺序变成官方客户端不会产生的那一种。要不要开，看上面「未命中都花在哪了」里「前缀每轮在变」那一栏占多少。',
            'Tool definitions and their order are both part of the prompt cache prefix. Some clients send the tool list in a different order every turn, which restarts the cache from zero each time; turning this on forces such a client to be stable. Off by default: the official codex CLI already sends a fixed order, so sorting gains nothing there and makes the forwarded order one the official client would never produce. To decide, look at how much the "Prefix keeps changing" row accounts for under "Where the misses went".',
          )}
          checked={data.normalize_tool_order}
          pending={toolOrder.isPending}
          onToggle={(on) => toolOrder.mutate(on)}
        />
      </SettingsGroup>

      <SettingsGroup
        icon={FingerprintIcon}
        title={t('上游指纹', 'Upstream fingerprint')}
        description={t(
          '转发出去的请求在上游看来是个什么客户端。',
          'What kind of client the forwarded request looks like from upstream.',
        )}
      >
        <ChoiceSetting
          label={t('发往上游的 User-Agent', 'User-Agent sent upstream')}
          description={t(
            '默认原样透传，不动来访客户端报的东西。要不要收敛取决于你的接入方是什么：coban 报给上游的 originator 是写死的官方 codex CLI，所以一个 OpenAI SDK 客户端接进来，透传就会造出「originator 说 codex CLI、UA 说 OpenAI/Python」这种官方客户端产生不出来的组合，走 /v1/chat/completions 那条路（请求体会被翻成 Responses 形状）更是必然如此——这类接入方存在时该开到「只改写不像官方客户端的」，来访确实是 codex CLI 的照旧透传（它报的版本可能比 coban 写死的更新）。「一律改写」留给确定只有翻译类客户端接入的场景。改写的目标值按账号派生，一个号一台稳定的机器、跨重启不变——不是整池共用一条 UA，那本身就是一簇可关联的指纹；改写时还会把 x-stainless-* 这类 SDK 留痕头一起清掉。三档都不管「来访压根没报 UA」那一格：那时一律补上该账号派生的那份，一个不带 UA 的客户端拿着订阅 token 打上游最显眼。账号页里「来访客户端」那一列显示的始终是客户端自报的原值，不受这个设置影响。',
            'Passes the caller\u2019s UA through untouched by default. Whether to converge depends on what connects to you: the originator coban reports upstream is hard-coded to the official codex CLI, so an OpenAI SDK client passed through makes upstream see "originator says codex CLI, UA says OpenAI/Python" — a combination the official client can never produce, and the /v1/chat/completions path (whose body is translated into Responses shape) is always like that. With such callers, switch to rewriting only what does not look official; a caller that really is the codex CLI still passes through, since its version may be newer than the one coban pins. "Always rewrite" is for setups where only translated clients connect. The replacement is derived per account, so each account is one stable machine across restarts — not one shared UA for the whole pool, which is a correlatable fingerprint in itself; rewriting also drops SDK trace headers such as x-stainless-*. None of the three settings covers a caller that sends no UA at all: that always gets the account\u2019s derived UA, because a client with no UA holding a subscription token is the most conspicuous thing there is. The "Client" column on the accounts page always shows what the caller reported, regardless of this setting.',
          )}
          value={data.upstream_ua_mode}
          options={[
            { value: 0, label: t('原样透传来访客户端的（默认）', 'Pass the caller\u2019s UA through (default)') },
            { value: 1, label: t('只改写不像官方客户端的', 'Rewrite only non-official UAs') },
            { value: 2, label: t('一律改写', 'Always rewrite') },
          ]}
          pending={uaMode.isPending}
          onSelect={(mode) => uaMode.mutate(mode)}
        />
      </SettingsGroup>
    </div>
  )
}

// ---------- 控制台安全 ----------

export function SecuritySettingsContent() {
  const { language, t } = useI18n()
  const authQuery = useQuery({ queryKey: ['auth-state'], queryFn: getAuthState })
  const [pw, setPwDraft] = useState('')
  const [confirm, setConfirm] = useState('')
  const [clearOpen, setClearOpen] = useState(false)

  const envManaged = authQuery.data?.env_managed ?? false
  const configured = authQuery.data?.configured ?? false

  const change = useMutation({
    mutationFn: async (password: string) => {
      if (configured) return changePassword(password)
      return setupPassword(password)
    },
    onSuccess: (_r, password) => {
      setPwDraft('')
      setConfirm('')
      setClearOpen(false)
      if (password) {
        setPw(password)
        toastManager.add({
          title: configured ? t('管理密码已更新', 'Admin password updated') : t('管理密码已设置', 'Admin password set'),
          type: 'success',
        })
      } else {
        clearPw()
        toastManager.add({ title: t('管理密码已清除', 'Admin password cleared'), type: 'success' })
      }
      // 首次设置后立即带上新密码；清除密码后让 App 回到未鉴权状态。
      window.location.reload()
    },
    onError: (error) => toastManager.add({
      title: t('操作失败', 'Operation failed'),
      description: extractError(error, language),
      type: 'error',
    }),
  })

  const mismatch = pw !== confirm
  const tooShort = pw.trim().length > 0 && pw.trim().length < 4
  const canSave = configured
    ? (!mismatch && !tooShort)
    : pw.trim().length >= 4 && !mismatch

  if (authQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner className="size-4" />
        {t('正在加载安全设置', 'Loading security settings')}
      </div>
    )
  }

  if (authQuery.isError || !authQuery.data) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">{t('无法读取登录状态', 'Unable to load sign-in status')}</p>
        <Button size="sm" variant="outline" loading={authQuery.isFetching} onClick={() => authQuery.refetch()}>
          {t('重试', 'Retry')}
        </Button>
      </div>
    )
  }

  return (
    <>
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
          <Alert className="m-5">
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
            onSubmit={(e) => {
              e.preventDefault()
              if (!canSave) return
              if (configured && pw.trim() === '') setClearOpen(true)
              else change.mutate(pw.trim())
            }}
            className="space-y-3 p-5"
          >
            <Field>
              <FieldLabel>
                {configured ? t('新密码', 'New password') : t('管理密码', 'Admin password')}
              </FieldLabel>
              <Input
                aria-label={configured ? t('新管理密码', 'New admin password') : t('管理密码', 'Admin password')}
                type="password"
                value={pw}
                autoComplete="new-password"
                onChange={(e) => setPwDraft(e.target.value)}
              />
              <FieldDescription>
                {configured
                  ? t('至少 4 位；留空并保存表示清除密码。', 'At least 4 characters. Save an empty value to clear the password.')
                  : t('至少 4 位；设置后控制台将要求登录。', 'At least 4 characters. The console will require sign-in once set.')}
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
              {confirm !== '' && mismatch && (
                <FieldDescription className="text-destructive">
                  {t('两次输入不一致', 'The two entries do not match')}
                </FieldDescription>
              )}
            </Field>
            <Button type="submit" disabled={change.isPending || !canSave}>
              {change.isPending && <Spinner />}
              {configured && pw.trim() === '' ? t('清除密码', 'Clear password') : configured ? t('保存密码', 'Save password') : t('设置密码', 'Set password')}
            </Button>
          </Form>
        )}
        </SettingsGroup>
      </div>
      <AlertDialog open={clearOpen} onOpenChange={(nextOpen) => { if (!change.isPending) setClearOpen(nextOpen) }}>
        <AlertDialogPopup className="sm:max-w-md">
          <AlertDialogHeader>
            <AlertDialogTitle>{t('清除管理密码', 'Clear admin password')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t('清除后，控制台将不再要求登录。', 'After clearing it, the console will no longer require sign-in.')}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={change.isPending} variant="ghost" />}>
              {t('取消', 'Cancel')}
            </AlertDialogClose>
            <Button loading={change.isPending} variant="destructive" onClick={() => change.mutate('')}>
              {t('确认清除', 'Clear password')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </>
  )
}
