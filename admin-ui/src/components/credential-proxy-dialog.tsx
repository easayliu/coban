import { useEffect, useState } from 'react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { GlobeIcon, MapPinIcon, PlayIcon, XIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { listProxies, testProxy, type ProxyTestResult, type SavedProxy } from '@/api/proxies'
import { type Language, useI18n } from '@/lib/i18n'
import { displayCredentialLabel, extractError } from '@/lib/utils'
import { proxyMaskedUrl, type CredentialActions } from '@/components/credential-shared'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import {
  Combobox,
  ComboboxItem,
  ComboboxPopup,
  ComboboxTrigger,
  ComboboxValue,
} from '@/components/ui/combobox'
import {
  Dialog,
  DialogClose,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

/**
 * 逐账号出站代理的编辑框。
 *
 * 清空输入框即改回直连——不额外做一个「清除」按钮：一个输入框两种语义，比两个入口更难
 * 点错。校验一律交给后端（[`crate::clients::validate_proxy`]），这里只负责把失败原文
 * 呈上来，免得前后端各写一套判据、哪天两边漂开。
 */
export function CredentialProxyDialog({
  cred,
  open,
  onOpenChange,
  proxy,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  proxy: CredentialActions['proxy']
}) {
  const { t, language } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [value, setValue] = useState(cred.proxy ?? '')

  // 每次打开都从服务端那份重置：上一次输了一半没保存就关掉的残留，下次打开还留着的话
  // 会让人以为它已经生效了。
  useEffect(() => {
    if (open) setValue(cred.proxy ?? '')
  }, [open, cred.proxy])

  const proxiesQuery = useQuery({ queryKey: ['proxies'], queryFn: listProxies, enabled: open })
  const savedProxies = proxiesQuery.data ?? []

  const trimmed = value.trim()
  const current = cred.proxy ?? ''
  const dirty = trimmed !== current

  const save = () => {
    proxy.mutate(trimmed === '' ? null : trimmed, {
      onSuccess: () => onOpenChange(false),
    })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t('出站代理', 'Outbound proxy')}</DialogTitle>
          <DialogDescription className="mt-1 truncate" title={credentialLabel}>
            {credentialLabel}
          </DialogDescription>
        </DialogHeader>

        <DialogPanel className="space-y-4">
          {savedProxies.length > 0 && (
            <div className="space-y-2">
              <Label>{t('从代理池选择', 'Pick from the proxy pool')}</Label>
              <ProxyPickerCombobox proxies={savedProxies} value={trimmed} onPick={setValue} />
            </div>
          )}

          <div className="space-y-2">
            <Label htmlFor="cred-proxy">{t('代理地址', 'Proxy URL')}</Label>
            <Input
              id="cred-proxy"
              value={value}
              onChange={(event) => setValue(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter' && dirty && !proxy.isPending) save()
              }}
              placeholder="socks5://127.0.0.1:1080"
              spellCheck={false}
              autoComplete="off"
            />
            <p className="text-muted-foreground text-xs leading-relaxed">
              {t(
                '支持 socks5://、socks5h://、http://、https://，可带 user:pass@（密码里的特殊字符要 percent-encode，如 # 写成 %23）。留空表示直连。',
                'Supports socks5://, socks5h://, http://, https://, optionally with user:pass@ (percent-encode special characters in the password, e.g. # as %23). Leave empty for a direct connection.',
              )}
            </p>
            <p className="text-muted-foreground text-xs leading-relaxed">
              {t(
                '填 socks5:// 会在保存时自动改成 socks5h://——让代理端解析域名，而不是在本机解析。本机解析会把上游域名泄露给本地 DNS，解析出的也是离你就近的 IP，而且不少住宅代理只接受域名形式、直接断连。socks4/socks4a 不再支持：SOCKS4 协议带不了账号密码，填了会被静默丢掉。',
                'socks5:// is rewritten to socks5h:// on save, so DNS is resolved at the proxy rather than locally. Local resolution leaks the upstream hostname to your DNS, yields an IP close to you rather than the proxy, and many residential proxies reject address-form requests outright. socks4/socks4a are no longer supported: the SOCKS4 protocol cannot carry a username and password, so credentials would be silently dropped.',
              )}
            </p>
          </div>

          <ProxyTestBlock url={trimmed} />

          <Alert>
            <GlobeIcon />
            <AlertDescription>
              {t(
                '配好之后，这个账号的全部出站流量都走它：转发、token 刷新、账号信息、连通性测试。代理不可用时该账号的请求会直接失败，不会退回直连——那样会把真实 IP 暴露给上游。',
                "Once set, all of this account's outbound traffic goes through it: forwarding, token refresh, profile, and connectivity tests. If the proxy is unusable the account's requests fail outright rather than falling back to a direct connection, which would expose your real IP upstream.",
              )}
            </AlertDescription>
          </Alert>
        </DialogPanel>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button onClick={save} disabled={!dirty || proxy.isPending}>
            {trimmed === '' ? t('改回直连', 'Use direct') : t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}

/**
 * 代理池下拉。选中一条就把它的 URL 填进输入框，保存仍走输入框那一份——池子只是目录。
 *
 * - `items` 必须传：Base UI 的「无匹配」与内置筛选都靠它，不传的话搜索框根本不过滤。
 * - 自定义 filter：名称常是随手起的，把原始地址与使用账号一并纳入匹配（用原始 URL
 *   而不是打码后的——人是照着自己配的地址找的，`***` 搜不到）。
 * - 地址打码显示，理由见 {@link proxyMaskedUrl}。
 */
export function ProxyPickerCombobox({
  proxies,
  value,
  onPick,
}: {
  proxies: SavedProxy[]
  value: string
  onPick: (url: string) => void
}) {
  const { t } = useI18n()
  const byId = (id: number) => proxies.find((p) => p.id === id)

  return (
    <Combobox
      items={proxies.map((p) => p.id)}
      value={proxies.find((p) => p.url === value)?.id ?? null}
      onValueChange={(id) => {
        const found = byId(id as number)
        if (found) onPick(found.url)
      }}
      itemToStringLabel={(id) => byId(id as number)?.label ?? ''}
      filter={(id, query) => {
        const q = query.trim().toLowerCase()
        if (!q) return true
        const p = byId(id as number)
        return !!p && `${p.label} ${p.url} ${p.credential_labels.join(' ')}`.toLowerCase().includes(q)
      }}
    >
      <ComboboxTrigger className="w-full min-w-0 flex-1">
        <ComboboxValue placeholder={t('选择代理…', 'Select a proxy…')} />
      </ComboboxTrigger>
      <ComboboxPopup
        className="max-h-80"
        inputPlaceholder={t('搜索名称、地址或使用账号…', 'Search name, URL, or account…')}
      >
        {(id: number) => {
          const p = byId(id)
          if (!p) return null
          return (
            <ComboboxItem key={p.id} value={p.id}>
              <div className="min-w-0">
                <div className="flex items-baseline gap-2">
                  <span className="truncate font-medium">{p.label}</span>
                  {p.credential_labels.length > 0 && (
                    <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
                      {t(`${p.credential_labels.length} 个账号`, `${p.credential_labels.length} acct`)}
                    </span>
                  )}
                </div>
                <div className="truncate font-mono text-xs text-muted-foreground">{proxyMaskedUrl(p.url)}</div>
              </div>
            </ComboboxItem>
          )
        }}
      </ComboboxPopup>
    </Combobox>
  )
}

/** 请求本身失败（网络错误、400 地址不合法）也折成一条失败结果，与代理不通走同一处展示。 */
export function failedProxyTest(url: string, error: string): ProxyTestResult {
  return { ok: false, proxy: url, status: 0, latency_ms: 0, ip: null, loc: null, colo: null, error }
}

/** 国家码 → 当前语言下的地区名（`JP` → 日本）；浏览器不认就原样给码。 */
function regionName(code: string, language: Language): string {
  try {
    return new Intl.DisplayNames([language], { type: 'region' }).of(code) ?? code
  } catch {
    return code
  }
}

/** 一条测试结果：通了给出口 IP、地区、机房与延迟，不通给原因。代理池页与本弹窗共用。 */
export function ProxyTestResultView({
  result,
  onDismiss,
}: {
  result: ProxyTestResult
  onDismiss?: () => void
}) {
  const { t, language } = useI18n()
  return (
    <div
      className={`flex items-start gap-2 rounded-md border px-3 py-2 text-xs ${
        result.ok ? 'border-success/30 bg-success/5' : 'border-destructive/30 bg-destructive/5'
      }`}
    >
      <MapPinIcon className="mt-0.5 size-3.5 shrink-0" />
      {result.ok ? (
        <div className="min-w-0 space-y-0.5">
          <p className="break-all font-medium font-mono">{result.ip}</p>
          <p className="text-muted-foreground">
            {[
              result.loc && regionName(result.loc, language),
              result.colo && t(`机房 ${result.colo}`, `colo ${result.colo}`),
              `${result.latency_ms}ms`,
            ].filter(Boolean).join(' · ')}
          </p>
        </div>
      ) : (
        <p className="min-w-0 break-all text-destructive-foreground">
          {result.error}
          {result.latency_ms > 0 && ` · ${result.latency_ms}ms`}
        </p>
      )}
      {onDismiss && (
        <Button
          size="icon-sm"
          variant="ghost"
          className="-my-0.5 ml-auto shrink-0"
          aria-label={t('关闭', 'Dismiss')}
          onClick={onDismiss}
        >
          <XIcon className="size-3" />
        </Button>
      )}
    </div>
  )
}

/** 「测试代理」按钮 + 结果。测的是输入框里当前的值，未保存的也能测。 */
export function ProxyTestBlock({ url }: { url: string }) {
  const { t, language } = useI18n()
  // 结果记下测的是哪个地址：改了输入框后，上一个地址的结果不能挂在新地址下面。
  const [tested, setTested] = useState<{ url: string; result: ProxyTestResult } | null>(null)
  const test = useMutation({
    mutationFn: (target: string) => testProxy(target),
    onSuccess: (result, target) => setTested({ url: target, result }),
    onError: (e, target) => setTested({ url: target, result: failedProxyTest(target, extractError(e, language)) }),
  })
  const result = tested?.url === url ? tested.result : null

  if (!url) return null

  return (
    <div className="space-y-2">
      <Button
        type="button"
        size="sm"
        variant="outline"
        loading={test.isPending && test.variables === url}
        onClick={() => test.mutate(url)}
      >
        <PlayIcon />
        {t('测试代理', 'Test proxy')}
      </Button>
      {result && <ProxyTestResultView result={result} onDismiss={() => setTested(null)} />}
    </div>
  )
}
