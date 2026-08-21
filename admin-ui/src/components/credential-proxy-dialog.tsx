import { useEffect, useState } from 'react'
import { GlobeIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { displayCredentialLabel } from '@/lib/utils'
import { type CredentialActions } from '@/components/credential-shared'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
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
