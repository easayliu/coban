import { useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { SearchIcon } from 'lucide-react'
import { listCredentials, setProxies } from '@/api/credentials'
import type { SavedProxy } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { displayCredentialLabel, extractError } from '@/lib/utils'
import { proxyMaskedUrl } from '@/components/credential-shared'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
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
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'

/**
 * 在代理池里直接调整「哪些账号走这条代理」。
 *
 * 勾上 = 该账号改用这条代理（原来走别的代理也一并改过来）；取消勾选 = 改回直连。
 * 保存时只提交有变化的账号，拆成「加入」「移出」两次批量调用，复用 `/credentials/proxy`。
 */
export function ProxyAccountsDialog({
  proxy,
  pool,
  open,
  onOpenChange,
}: {
  proxy: SavedProxy
  /** 整个代理池，用来把其它账号当前的代理地址翻成名称。 */
  pool: SavedProxy[]
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  // 与主页面共用 ['credentials'] 缓存，打开弹窗时通常已有数据。
  const credsQuery = useQuery({ queryKey: ['credentials'], queryFn: listCredentials, enabled: open })
  const creds = credsQuery.data ?? []

  const [query, setQuery] = useState('')
  // 只记用户改动过的勾选状态；没改过的按账号当前配置显示，数据刷新后不会被旧快照盖掉。
  const [overrides, setOverrides] = useState<Map<number, boolean>>(() => new Map())

  const usesThis = (proxyUrl: string | null) => proxyUrl === proxy.url
  const isChecked = (id: number, proxyUrl: string | null) => overrides.get(id) ?? usesThis(proxyUrl)

  // 正在用这条代理的排在前面，其余按原顺序；排序只看当前配置，勾选时行不跳动。
  const sorted = useMemo(
    () => [...creds].sort((a, b) => Number(usesThis(b.proxy)) - Number(usesThis(a.proxy))),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [creds, proxy.url],
  )
  const q = query.trim().toLowerCase()
  const visible = q
    ? sorted.filter((c) =>
        `${c.label} ${displayCredentialLabel(c.label, language)}`.toLowerCase().includes(q),
      )
    : sorted

  const toAdd = creds.filter((c) => overrides.get(c.id) === true && !usesThis(c.proxy))
  const toRemove = creds.filter((c) => overrides.get(c.id) === false && usesThis(c.proxy))
  const dirty = toAdd.length > 0 || toRemove.length > 0

  const reset = () => {
    setOverrides(new Map())
    setQuery('')
  }

  const save = useMutation({
    mutationFn: async () => {
      if (toAdd.length > 0) await setProxies(toAdd.map((c) => c.id), proxy.url)
      if (toRemove.length > 0) await setProxies(toRemove.map((c) => c.id), null)
    },
    onSuccess: () => {
      toastManager.add({ title: t('已更新使用账号', 'Accounts updated'), type: 'success' })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      qc.invalidateQueries({ queryKey: ['proxies'] })
      onOpenChange(false)
      reset()
    },
    onError: (e) => {
      // 可能「加入」已成功、「移出」失败，刷新一次让界面与实际配置对齐。
      qc.invalidateQueries({ queryKey: ['credentials'] })
      qc.invalidateQueries({ queryKey: ['proxies'] })
      toastManager.add({
        title: t('更新使用账号失败', 'Failed to update accounts'),
        description: extractError(e, language),
        type: 'error',
      })
    },
  })

  const currentProxyText = (url: string | null) => {
    if (!url) return t('当前直连', 'Currently direct')
    const name = pool.find((p) => p.url === url)?.label ?? proxyMaskedUrl(url)
    return t(`当前：${name}`, `Currently: ${name}`)
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        onOpenChange(next)
        if (!next) reset()
      }}
    >
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('使用账号', 'Accounts using this proxy')}</DialogTitle>
          <DialogDescription className="mt-1 truncate" title={proxy.label}>
            {proxy.label}
          </DialogDescription>
        </DialogHeader>

        <DialogPanel className="space-y-3">
          {credsQuery.isPending ? (
            <div className="flex min-h-24 items-center justify-center gap-2 text-sm text-muted-foreground">
              <Spinner className="size-4" />
              {t('正在加载', 'Loading')}
            </div>
          ) : creds.length === 0 ? (
            <p className="py-6 text-center text-sm text-muted-foreground">
              {t('还没有账号。', 'No accounts yet.')}
            </p>
          ) : (
            <>
              {creds.length > 8 && (
                <div className="relative">
                  <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
                  <Input
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                    placeholder={t('搜索账号…', 'Search accounts…')}
                    className="pl-7"
                    size="sm"
                  />
                </div>
              )}
              <ul className="max-h-80 divide-y overflow-y-auto rounded-md border" role="list">
                {visible.map((c) => {
                  const checked = isChecked(c.id, c.proxy)
                  return (
                    <li key={c.id}>
                      <label className="flex cursor-pointer items-center gap-3 px-3 py-2 hover:bg-accent/50">
                        <Checkbox
                          checked={checked}
                          onCheckedChange={(next) =>
                            setOverrides((prev) => new Map(prev).set(c.id, next === true))
                          }
                        />
                        <div className="min-w-0 flex-1">
                          <p className="truncate text-sm">{displayCredentialLabel(c.label, language)}</p>
                          {!usesThis(c.proxy) && (
                            <p className="truncate text-xs text-muted-foreground">
                              {currentProxyText(c.proxy)}
                            </p>
                          )}
                        </div>
                        {c.disabled && (
                          <span className="shrink-0 text-xs text-muted-foreground">
                            {t('已停用', 'Disabled')}
                          </span>
                        )}
                      </label>
                    </li>
                  )
                })}
                {visible.length === 0 && (
                  <li className="px-3 py-4 text-center text-sm text-muted-foreground">
                    {t('无匹配结果', 'No matches')}
                  </li>
                )}
              </ul>
              {toRemove.length > 0 && (
                <Alert>
                  <AlertDescription>
                    {t(
                      `取消勾选的 ${toRemove.length} 个账号会改回直连，出站流量将使用本机真实 IP。`,
                      `${toRemove.length} unchecked account${toRemove.length === 1 ? '' : 's'} will switch to a direct connection and use this server’s real IP.`,
                    )}
                  </AlertDescription>
                </Alert>
              )}
            </>
          )}
        </DialogPanel>

        <DialogFooter>
          {dirty && (
            <p className="mr-auto self-center text-xs text-muted-foreground tabular-nums">
              {t(
                `加入 ${toAdd.length} 个，移出 ${toRemove.length} 个`,
                `${toAdd.length} to add, ${toRemove.length} to remove`,
              )}
            </p>
          )}
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button onClick={() => save.mutate()} disabled={!dirty} loading={save.isPending}>
            {t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
