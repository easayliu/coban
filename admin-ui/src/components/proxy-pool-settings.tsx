import { Fragment, useCallback, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  CheckIcon,
  CopyIcon,
  DownloadIcon,
  FileUpIcon,
  GlobeIcon,
  PencilIcon,
  PlayIcon,
  PlusIcon,
  RotateCwIcon,
  Trash2Icon,
  UsersIcon,
  XIcon,
} from 'lucide-react'
import {
  addProxy,
  deleteProxies,
  importProxies,
  listProxies,
  type ProxyImportScheme,
  type ProxyTestResult,
  type SavedProxy,
  testProxy,
  updateProxy,
} from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { copyText, extractError } from '@/lib/utils'
import { proxyMaskedUrl } from '@/components/credential-shared'
import { failedProxyTest, ProxyTestResultView } from '@/components/credential-proxy-dialog'
import { ProxyAccountsDialog } from '@/components/proxy-accounts-dialog'
import { SettingsGroup } from '@/components/settings-group'
import {
  AlertDialog,
  AlertDialogClose,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogPopup,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Badge } from '@/components/ui/badge'
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
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import { toastManager } from '@/components/ui/toast'

/**
 * 批量测试的并发数。每条最长 15s，全串行几十条要等好几分钟；并发太高又是同一台机器
 * 同时从几十个出口打 chatgpt.com，住宅代理那头也容易被判成滥用。
 */
const BATCH_TEST_CONCURRENCY = 4

/** 按最近一次测试结果筛。结果只在本页内存里，刷新页面就回到全部「未测」。 */
type StatusFilter = 'all' | 'ok' | 'failed' | 'untested'

/** 删除确认框的目标：一条，或一批（失败项 / 勾选项）。 */
type DeleteTarget = { kind: 'one' | 'failed' | 'selected'; proxies: SavedProxy[] }

export function ProxyPoolSettingsContent() {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const proxiesQuery = useQuery({ queryKey: ['proxies'], queryFn: listProxies })

  const [addLabel, setAddLabel] = useState('')
  const [addUrl, setAddUrl] = useState('')

  // 测试结果按地址记，提在页面这一层：添加框、每一行、批量测试三处共用一份，添加前测过的
  // 结果随新行一起带下去。**不入库**：出口会变，一份昨天的「通」比没有更误导。
  const [results, setResults] = useState<Record<string, ProxyTestResult>>({})
  const [testing, setTesting] = useState<Set<string>>(() => new Set())
  // 记下是哪个批量按钮在跑：几个按钮互斥，但转圈只转被点的那个。
  const [batchRunning, setBatchRunning] = useState<'all' | 'failed' | 'selected' | null>(null)
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all')
  const [selected, setSelected] = useState<Set<number>>(() => new Set())
  const [deleteTarget, setDeleteTarget] = useState<DeleteTarget | null>(null)
  const [importOpen, setImportOpen] = useState(false)
  const [exportOpen, setExportOpen] = useState(false)

  const runTest = useCallback(
    async (url: string): Promise<ProxyTestResult> => {
      setTesting((prev) => new Set(prev).add(url))
      let result: ProxyTestResult
      try {
        result = await testProxy(url)
      } catch (e) {
        result = failedProxyTest(url, extractError(e, language))
      }
      setResults((prev) => ({ ...prev, [url]: result }))
      setTesting((prev) => {
        const next = new Set(prev)
        next.delete(url)
        return next
      })
      return result
    },
    [language],
  )
  const dismissResult = (url: string) =>
    setResults((prev) => {
      const next = { ...prev }
      delete next[url]
      return next
    })

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: ['proxies'] })
    qc.invalidateQueries({ queryKey: ['credentials'] })
  }
  const onError = (title: string, error: unknown) =>
    toastManager.add({ title, description: extractError(error, language), type: 'error' })

  const trimmedAddUrl = addUrl.trim()
  const addResult = results[trimmedAddUrl]

  const create = useMutation({
    mutationFn: () => addProxy(addLabel.trim(), trimmedAddUrl),
    onSuccess: (p) => {
      toastManager.add({ title: t('已添加代理', 'Proxy added'), description: p.label, type: 'success' })
      // 后端可能把地址归一化（socks5:// → socks5h://），测试结果改挂到入库后的地址上。
      if (addResult) {
        setResults((prev) => {
          const next = { ...prev, [p.url]: addResult }
          if (p.url !== trimmedAddUrl) delete next[trimmedAddUrl]
          return next
        })
      }
      setAddLabel('')
      setAddUrl('')
      invalidate()
    },
    onError: (e) => onError(t('添加代理失败', 'Failed to add proxy'), e),
  })

  const remove = useMutation({
    mutationFn: (target: DeleteTarget) => deleteProxies(target.proxies.map((p) => p.id)),
    onSuccess: (deleted, target) => {
      setResults((prev) => {
        const next = { ...prev }
        for (const p of target.proxies) delete next[p.url]
        return next
      })
      setSelected((prev) => {
        const next = new Set(prev)
        for (const p of target.proxies) next.delete(p.id)
        return next
      })
      setDeleteTarget(null)
      // 筛选跟着这批失败项一起结束：不复位的话，下次测出失败会一下子只剩失败项，像是代理被删了。
      if (target.kind === 'failed') setStatusFilter('all')
      invalidate()
      toastManager.add({
        title: t(`已删除 ${deleted} 条代理`, `Deleted ${deleted} prox${deleted === 1 ? 'y' : 'ies'}`),
        type: 'success',
      })
    },
    onError: (e) => {
      setDeleteTarget(null)
      onError(t('删除代理失败', 'Failed to delete proxies'), e)
    },
  })

  const testMany = async (urls: string[], kind: 'all' | 'failed' | 'selected') => {
    setBatchRunning(kind)
    const queue = [...urls]
    let ok = 0
    let failed = 0
    await Promise.all(
      Array.from({ length: Math.min(BATCH_TEST_CONCURRENCY, queue.length) }, async () => {
        for (let url = queue.shift(); url !== undefined; url = queue.shift()) {
          if ((await runTest(url)).ok) ok++
          else failed++
        }
      }),
    )
    setBatchRunning(null)
    toastManager.add({
      title: t('测试完成', 'Test finished'),
      description: t(`${ok} 条可用，${failed} 条不可用`, `${ok} working, ${failed} failed`),
      type: failed === 0 ? 'success' : 'warning',
    })
  }

  if (proxiesQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner className="size-4" />
        {t('正在加载', 'Loading')}
      </div>
    )
  }

  if (proxiesQuery.isError) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">{t('无法读取代理池', 'Unable to load the proxy pool')}</p>
        <p className="text-xs text-muted-foreground">{extractError(proxiesQuery.error, language)}</p>
        <Button size="sm" variant="outline" loading={proxiesQuery.isFetching} onClick={() => proxiesQuery.refetch()}>
          {t('重试', 'Retry')}
        </Button>
      </div>
    )
  }

  const proxies = proxiesQuery.data ?? []
  const testedCount = proxies.filter((p) => results[p.url]).length
  const okCount = proxies.filter((p) => results[p.url]?.ok).length
  const failedProxies = proxies.filter((p) => results[p.url]?.ok === false)
  const byStatus: Record<StatusFilter, SavedProxy[]> = {
    all: proxies,
    ok: proxies.filter((p) => results[p.url]?.ok),
    failed: failedProxies,
    untested: proxies.filter((p) => !results[p.url]),
  }
  // 筛到的那一类清空后（失败项重测都通了 / 删光了）自动回到全部，不留一个空列表——
  // 看着像代理被删了。
  const activeFilter: StatusFilter = byStatus[statusFilter].length > 0 ? statusFilter : 'all'
  const visibleProxies = byStatus[activeFilter]
  const filterLabels: Record<StatusFilter, string> = {
    all: t('全部', 'All'),
    ok: t('可用', 'Working'),
    failed: t('失败', 'Failed'),
    untested: t('未测', 'Untested'),
  }
  // 勾选只认还在池里的：别处删掉的条目不能留在「已选 N 条」里。
  const selectedProxies = proxies.filter((p) => selected.has(p.id))
  const allVisibleSelected = visibleProxies.length > 0 && visibleProxies.every((p) => selected.has(p.id))
  const someVisibleSelected = visibleProxies.some((p) => selected.has(p.id))

  const toggleAllVisible = () =>
    setSelected((prev) => {
      const next = new Set(prev)
      for (const p of visibleProxies) {
        if (allVisibleSelected) next.delete(p.id)
        else next.add(p.id)
      }
      return next
    })

  return (
    <div className="space-y-4">
      <SettingsGroup
        icon={GlobeIcon}
        title={t('代理池', 'Proxy pool')}
        description={t(
          '集中管理可复用的出站代理。添加后可在账号的「出站代理」里直接选取，也可以在这里把一条代理分给多个账号。测试会经代理访问 chatgpt.com，不占用任何账号的额度。',
          'Manage reusable outbound proxies. Pick them from an account’s “Outbound proxy” dialog, or assign one to many accounts from here. Tests reach chatgpt.com through the proxy and use no account quota.',
        )}
      >
        <Form
          className="flex flex-wrap items-end gap-2 px-4 py-4 sm:px-5"
          onSubmit={(event) => {
            event.preventDefault()
            if (trimmedAddUrl && !create.isPending) create.mutate()
          }}
        >
          <div className="min-w-0 flex-1 space-y-1 max-sm:basis-full">
            <label className="text-xs font-medium" htmlFor="proxy-pool-add-label">{t('名称', 'Name')}</label>
            <Input
              id="proxy-pool-add-label"
              value={addLabel}
              onChange={(event) => setAddLabel(event.target.value)}
              placeholder={t('留空按 host:port 命名', 'Defaults to host:port')}
              size="sm"
            />
          </div>
          <div className="min-w-0 flex-[2] space-y-1">
            <label className="text-xs font-medium" htmlFor="proxy-pool-add-url">{t('代理地址', 'Proxy URL')}</label>
            <Input
              id="proxy-pool-add-url"
              value={addUrl}
              onChange={(event) => setAddUrl(event.target.value)}
              placeholder="socks5://user:pass@127.0.0.1:1080"
              spellCheck={false}
              autoComplete="off"
              size="sm"
            />
          </div>
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={!trimmedAddUrl}
            loading={testing.has(trimmedAddUrl)}
            onClick={() => runTest(trimmedAddUrl)}
          >
            <PlayIcon />
            {t('测试', 'Test')}
          </Button>
          <Button type="submit" size="sm" disabled={!trimmedAddUrl} loading={create.isPending}>
            <PlusIcon />
            {t('添加', 'Add')}
          </Button>
          {addResult && (
            <div className="basis-full">
              <ProxyTestResultView result={addResult} onDismiss={() => dismissResult(trimmedAddUrl)} />
            </div>
          )}
        </Form>

        <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2 border-t px-4 py-2.5 sm:px-5">
          <div className="flex items-center gap-3">
            {proxies.length > 0 && (
              <Checkbox
                aria-label={t('全选', 'Select all')}
                checked={allVisibleSelected}
                indeterminate={!allVisibleSelected && someVisibleSelected}
                onCheckedChange={toggleAllVisible}
              />
            )}
            <p className="text-xs text-muted-foreground tabular-nums">
              {selectedProxies.length > 0
                ? t(`已选 ${selectedProxies.length} / ${proxies.length} 条`, `${selectedProxies.length} of ${proxies.length} selected`)
                : testedCount > 0
                  ? t(
                    `共 ${proxies.length} 条 · 已测 ${testedCount} 条，${okCount} 条可用`,
                    `${proxies.length} total · ${testedCount} tested, ${okCount} working`,
                  )
                  : t(`共 ${proxies.length} 条`, `${proxies.length} total`)}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            {selectedProxies.length > 0 ? (
              <>
                <Button
                  size="sm"
                  variant="outline"
                  loading={batchRunning === 'selected'}
                  disabled={batchRunning !== null}
                  onClick={() => testMany(selectedProxies.map((p) => p.url), 'selected')}
                >
                  <PlayIcon />
                  {t('测试', 'Test')}
                </Button>
                <Button size="sm" variant="outline" onClick={() => setExportOpen(true)}>
                  <DownloadIcon />
                  {t('导出', 'Export')}
                </Button>
                <Button
                  size="sm"
                  variant="destructive-outline"
                  disabled={batchRunning !== null}
                  onClick={() => setDeleteTarget({ kind: 'selected', proxies: selectedProxies })}
                >
                  <Trash2Icon />
                  {t('删除', 'Delete')}
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setSelected(new Set())}>
                  {t('取消选择', 'Clear')}
                </Button>
              </>
            ) : (
              <>
                <Button size="sm" variant="outline" onClick={() => setImportOpen(true)}>
                  <FileUpIcon />
                  {t('批量导入', 'Import')}
                </Button>
                {proxies.length > 0 && (
                  <>
                    <Button size="sm" variant="outline" onClick={() => setExportOpen(true)}>
                      <DownloadIcon />
                      {t('导出', 'Export')}
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      loading={batchRunning === 'all'}
                      disabled={batchRunning !== null}
                      onClick={() => testMany(proxies.map((p) => p.url), 'all')}
                    >
                      <PlayIcon />
                      {t('全部测试', 'Test all')}
                    </Button>
                  </>
                )}
              </>
            )}
          </div>
        </div>

        {/* 测过才有得筛：一条没测时四个选项里三个是 0，只是占地方。「全选」只勾筛出来的这些，
            于是「筛可用 → 全选 → 导出 / 分给账号」一路能走通。 */}
        {testedCount > 0 && (
          <div className="flex items-center gap-2 border-t px-4 py-2.5 sm:px-5">
            <ToggleGroup
              value={[activeFilter]}
              onValueChange={(values) => {
                const next = values[values.length - 1] as StatusFilter | undefined
                if (next) setStatusFilter(next)
              }}
              variant="outline"
              size="sm"
              aria-label={t('按测试结果筛选', 'Filter by test result')}
            >
              {(Object.keys(filterLabels) as StatusFilter[]).map((key, i) => (
                <Fragment key={key}>
                  {i > 0 && <ToggleGroupSeparator />}
                  <ToggleGroupItem value={key} disabled={byStatus[key].length === 0} className="px-2.5 text-xs">
                    {filterLabels[key]}
                    <span className="text-muted-foreground tabular-nums">{byStatus[key].length}</span>
                  </ToggleGroupItem>
                </Fragment>
              ))}
            </ToggleGroup>
          </div>
        )}

        {/* 失败项单独一行：筛选、重测、删除都只针对这批，和上面的全池操作分开，免得误删。 */}
        {failedProxies.length > 0 && (
          <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2 bg-destructive/5 px-4 py-2.5 sm:px-5">
            <p className="text-xs font-medium text-destructive-foreground tabular-nums">
              {t(`${failedProxies.length} 条测试失败`, `${failedProxies.length} failed`)}
            </p>
            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant="outline"
                loading={batchRunning === 'failed'}
                disabled={batchRunning !== null}
                onClick={() => testMany(failedProxies.map((p) => p.url), 'failed')}
              >
                <RotateCwIcon />
                {t('重测', 'Retest')}
              </Button>
              <Button
                size="sm"
                variant="destructive-outline"
                disabled={batchRunning !== null}
                onClick={() => setDeleteTarget({ kind: 'failed', proxies: failedProxies })}
              >
                <Trash2Icon />
                {t('删除', 'Delete')}
              </Button>
            </div>
          </div>
        )}

        {proxies.length === 0 ? (
          <p className="border-t px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
            {t(
              '代理池还是空的。在上方填写地址添加第一条，或用「批量导入」一次粘贴多条。',
              'The proxy pool is empty. Add one above, or paste many at once with “Import”.',
            )}
          </p>
        ) : (
          <ul className="divide-y border-t" role="list">
            {visibleProxies.map((proxy) => (
              <ProxyRow
                key={proxy.id}
                proxy={proxy}
                pool={proxies}
                result={results[proxy.url]}
                testing={testing.has(proxy.url)}
                selected={selected.has(proxy.id)}
                onSelect={(on) =>
                  setSelected((prev) => {
                    const next = new Set(prev)
                    if (on) next.add(proxy.id)
                    else next.delete(proxy.id)
                    return next
                  })
                }
                onTest={() => runTest(proxy.url)}
                onDismissResult={() => dismissResult(proxy.url)}
                onDelete={() => setDeleteTarget({ kind: 'one', proxies: [proxy] })}
                onUrlChanged={(from, to) =>
                  setResults((prev) => {
                    const next = { ...prev }
                    delete next[from]
                    delete next[to]
                    return next
                  })
                }
              />
            ))}
          </ul>
        )}
      </SettingsGroup>

      <DeleteProxiesDialog
        target={deleteTarget}
        pending={remove.isPending}
        onOpenChange={(open) => { if (!open) setDeleteTarget(null) }}
        onConfirm={(target) => remove.mutate(target)}
      />
      <ImportProxiesDialog open={importOpen} onOpenChange={setImportOpen} />
      <ExportProxiesDialog
        open={exportOpen}
        onOpenChange={setExportOpen}
        proxies={selectedProxies.length > 0 ? selectedProxies : proxies}
        scoped={selectedProxies.length > 0}
      />
    </div>
  )
}

function ProxyRow({
  proxy,
  pool,
  result,
  testing,
  selected,
  onSelect,
  onTest,
  onDismissResult,
  onDelete,
  onUrlChanged,
}: {
  proxy: SavedProxy
  pool: SavedProxy[]
  result: ProxyTestResult | undefined
  testing: boolean
  selected: boolean
  onSelect: (on: boolean) => void
  onTest: () => void
  onDismissResult: () => void
  onDelete: () => void
  /** 地址改了，两个地址上挂着的旧测试结果都不再作数。 */
  onUrlChanged: (from: string, to: string) => void
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const [editing, setEditing] = useState(false)
  const [label, setLabel] = useState(proxy.label)
  const [url, setUrl] = useState(proxy.url)
  const [accountsOpen, setAccountsOpen] = useState(false)
  const inUse = proxy.credential_labels.length

  const edit = useMutation({
    mutationFn: () => updateProxy(proxy.id, label.trim(), url.trim()),
    onSuccess: (saved) => {
      toastManager.add({
        title: t('已更新代理', 'Proxy updated'),
        description: saved.url !== proxy.url && inUse > 0
          ? t(`${inUse} 个使用它的账号已一并改到新地址`, `${inUse} account(s) using it now use the new URL`)
          : undefined,
        type: 'success',
      })
      if (saved.url !== proxy.url) onUrlChanged(proxy.url, saved.url)
      setEditing(false)
      qc.invalidateQueries({ queryKey: ['proxies'] })
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (e) =>
      toastManager.add({
        title: t('更新代理失败', 'Failed to update proxy'),
        description: extractError(e, language),
        type: 'error',
      }),
  })

  if (editing) {
    return (
      <li className="flex flex-wrap items-end gap-2 px-4 py-4 sm:px-5">
        <div className="min-w-0 flex-1 space-y-1 max-sm:basis-full">
          <label className="text-xs font-medium" htmlFor={`proxy-edit-label-${proxy.id}`}>{t('名称', 'Name')}</label>
          <Input
            id={`proxy-edit-label-${proxy.id}`}
            value={label}
            onChange={(event) => setLabel(event.target.value)}
            size="sm"
            autoFocus
          />
        </div>
        <div className="min-w-0 flex-[2] space-y-1">
          <label className="text-xs font-medium" htmlFor={`proxy-edit-url-${proxy.id}`}>{t('代理地址', 'Proxy URL')}</label>
          <Input
            id={`proxy-edit-url-${proxy.id}`}
            value={url}
            onChange={(event) => setUrl(event.target.value)}
            spellCheck={false}
            autoComplete="off"
            size="sm"
          />
        </div>
        <Button
          size="icon-sm"
          variant="outline"
          loading={edit.isPending}
          disabled={!label.trim() || !url.trim()}
          onClick={() => edit.mutate()}
          aria-label={t('保存', 'Save')}
        >
          <CheckIcon />
        </Button>
        <Button
          size="icon-sm"
          variant="ghost"
          onClick={() => {
            setEditing(false)
            setLabel(proxy.label)
            setUrl(proxy.url)
          }}
          aria-label={t('取消', 'Cancel')}
        >
          <XIcon />
        </Button>
        {inUse > 0 && url.trim() !== proxy.url && (
          <p className="basis-full text-xs text-muted-foreground">
            {t(
              `保存后，正在使用它的 ${inUse} 个账号会一并改用新地址。`,
              `On save, the ${inUse} account(s) using it switch to the new URL too.`,
            )}
          </p>
        )}
      </li>
    )
  }

  return (
    <li className="space-y-2 px-4 py-3.5 sm:px-5">
      <div className="flex items-center gap-3">
        <Checkbox
          aria-label={t(`选择 ${proxy.label}`, `Select ${proxy.label}`)}
          checked={selected}
          onCheckedChange={(next) => onSelect(next === true)}
        />
        <div className="min-w-0 flex-1">
          <span className="block truncate text-sm font-medium">{proxy.label}</span>
          <p className="mt-0.5 truncate font-mono text-xs text-muted-foreground" title={proxyMaskedUrl(proxy.url)}>
            {proxyMaskedUrl(proxy.url)}
          </p>
        </div>
        <Button size="icon-sm" variant="ghost" loading={testing} onClick={onTest} aria-label={t('测试', 'Test')} title={t('测试', 'Test')}>
          <PlayIcon />
        </Button>
        <Button
          size="icon-sm"
          variant="ghost"
          onClick={() => setAccountsOpen(true)}
          aria-label={t('调整使用账号', 'Manage accounts')}
          title={t('调整使用账号', 'Manage accounts')}
        >
          <UsersIcon />
        </Button>
        <Button
          size="icon-sm"
          variant="ghost"
          onClick={() => {
            setLabel(proxy.label)
            setUrl(proxy.url)
            setEditing(true)
          }}
          aria-label={t('编辑', 'Edit')}
          title={t('编辑', 'Edit')}
        >
          <PencilIcon />
        </Button>
        <Button size="icon-sm" variant="ghost" onClick={onDelete} aria-label={t('删除', 'Delete')} title={t('删除', 'Delete')}>
          <Trash2Icon />
        </Button>
      </div>
      {inUse > 0 && (
        <p className="flex flex-wrap items-center gap-1 pl-7 text-xs text-muted-foreground">
          <span className="tabular-nums">{t(`使用账号（${inUse}）：`, `Used by (${inUse}): `)}</span>
          {proxy.credential_labels.map((name, i) => (
            <Badge key={i} variant="outline" size="sm">{name}</Badge>
          ))}
        </p>
      )}
      {result && (
        <div className="pl-7">
          <ProxyTestResultView result={result} onDismiss={onDismissResult} />
        </div>
      )}

      <ProxyAccountsDialog proxy={proxy} pool={pool} open={accountsOpen} onOpenChange={setAccountsOpen} />
    </li>
  )
}

function DeleteProxiesDialog({
  target,
  pending,
  onOpenChange,
  onConfirm,
}: {
  target: DeleteTarget | null
  pending: boolean
  onOpenChange: (open: boolean) => void
  onConfirm: (target: DeleteTarget) => void
}) {
  const { t } = useI18n()
  const list = target?.proxies ?? []
  const inUse = list.filter((p) => p.credential_labels.length > 0)
  const inUseAccounts = inUse.reduce((n, p) => n + p.credential_labels.length, 0)
  const title = target?.kind === 'one'
    ? t(`删除代理「${list[0]?.label}」`, `Delete proxy “${list[0]?.label}”`)
    : target?.kind === 'failed'
      ? t(`删除 ${list.length} 条测试失败的代理`, `Delete ${list.length} failed prox${list.length === 1 ? 'y' : 'ies'}`)
      : t(`删除选中的 ${list.length} 条代理`, `Delete ${list.length} selected prox${list.length === 1 ? 'y' : 'ies'}`)

  return (
    <AlertDialog open={target !== null} onOpenChange={onOpenChange}>
      <AlertDialogPopup>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>
            {inUse.length > 0
              ? t(
                `其中 ${inUse.length} 条仍被 ${inUseAccounts} 个账号使用。删除只是从代理池移除，这些账号的代理设置不变，仍会走原来的地址；需要换的话先在「使用账号」里调整。`,
                `${inUse.length} of them ${inUse.length === 1 ? 'is' : 'are'} still used by ${inUseAccounts} account${inUseAccounts === 1 ? '' : 's'}. Deleting only removes them from the pool; those accounts keep using the same URL. Reassign them under “Manage accounts” first if needed.`,
              )
              : t('没有账号在用，删除后从代理池移除。', 'No accounts use them. They will be removed from the pool.')}
          </AlertDialogDescription>
        </AlertDialogHeader>
        {target?.kind !== 'one' && (
          <ul className="max-h-48 space-y-1 overflow-y-auto px-4 pb-4 text-sm sm:px-6" role="list">
            {list.map((p) => (
              <li key={p.id} className="flex items-baseline gap-2">
                <span className="truncate">{p.label}</span>
                {p.credential_labels.length > 0 && (
                  <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
                    {t(`${p.credential_labels.length} 个账号在用`, `${p.credential_labels.length} in use`)}
                  </span>
                )}
              </li>
            ))}
          </ul>
        )}
        <AlertDialogFooter>
          <AlertDialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</AlertDialogClose>
          <Button
            variant="destructive"
            loading={pending}
            // 弹窗开着时重测可能把失败项清空，空列表发出去后端会回 400。
            disabled={list.length === 0}
            onClick={() => target && onConfirm(target)}
          >
            {t('删除', 'Delete')}
          </Button>
        </AlertDialogFooter>
      </AlertDialogPopup>
    </AlertDialog>
  )
}

function ImportProxiesDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const [text, setText] = useState('')
  const [scheme, setScheme] = useState<ProxyImportScheme>('http')
  const schemes: { label: string; value: ProxyImportScheme }[] = [
    { label: 'http://', value: 'http' },
    { label: 'https://', value: 'https' },
    { label: 'socks5h://', value: 'socks5h' },
  ]

  const run = useMutation({
    mutationFn: () => importProxies(text, scheme),
    onSuccess: (report) => {
      qc.invalidateQueries({ queryKey: ['proxies'] })
      const invalid = report.invalid.length
      toastManager.add({
        title: t(`已导入 ${report.added} 条`, `Imported ${report.added}`),
        description: [
          report.duplicated > 0 && t(`${report.duplicated} 条已在池中，跳过`, `${report.duplicated} already in the pool`),
          invalid > 0 && t(
            `${invalid} 行无法识别：${report.invalid.slice(0, 3).map(([n, e]) => `第 ${n} 行 ${e}`).join('；')}`,
            `${invalid} unrecognized: ${report.invalid.slice(0, 3).map(([n, e]) => `line ${n}: ${e}`).join('; ')}`,
          ),
        ].filter(Boolean).join(t('。', '. ')) || undefined,
        type: invalid > 0 ? 'warning' : 'success',
      })
      // 有坏行时把它们留在框里，改了再导一次；全成功才清空关掉。
      if (invalid > 0) {
        const bad = new Set(report.invalid.map(([n]) => n))
        setText(text.split('\n').filter((_, i) => bad.has(i + 1)).join('\n'))
      } else {
        setText('')
        onOpenChange(false)
      }
    },
    onError: (e) =>
      toastManager.add({ title: t('导入失败', 'Import failed'), description: extractError(e, language), type: 'error' }),
  })

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-xl">
        <DialogHeader>
          <DialogTitle>{t('批量导入代理', 'Import proxies')}</DialogTitle>
          <DialogDescription>
            {t(
              '一行一条，可在地址后空一格写名称；空行和 # 开头的行会跳过。已在池中的地址不会重复添加。',
              'One per line, optionally followed by a space and a name. Blank lines and lines starting with # are skipped. URLs already in the pool are left alone.',
            )}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel className="space-y-4">
          <Textarea
            value={text}
            onChange={(event) => setText(event.target.value)}
            rows={10}
            spellCheck={false}
            className="font-mono text-xs"
            placeholder={'socks5://user:pass@1.2.3.4:1080 日本 1\nhttp://5.6.7.8:8080\n9.9.9.9:3128:user:pass'}
          />
          <div className="space-y-2">
            <Label>{t('未写协议的行按', 'Lines without a scheme use')}</Label>
            <Select items={schemes} value={scheme} onValueChange={(v) => v && setScheme(v as ProxyImportScheme)}>
              <SelectTrigger className="w-40">
                <SelectValue />
              </SelectTrigger>
              <SelectPopup>
                {schemes.map((s) => (
                  <SelectItem key={s.value} value={s.value}>{s.label}</SelectItem>
                ))}
              </SelectPopup>
            </Select>
            <p className="text-xs leading-relaxed text-muted-foreground">
              {t(
                '支持 host:port、user:pass@host:port 与代理商常见的 host:port:user:pass；最后这种写法里的账号密码会自动转义，不用手动处理 # @ 这类字符。socks5:// 会存成 socks5h://。',
                'Accepts host:port, user:pass@host:port, and the common vendor format host:port:user:pass — credentials in the last form are escaped for you, so # or @ in a password is fine. socks5:// is stored as socks5h://.',
              )}
            </p>
          </div>
        </DialogPanel>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button disabled={!text.trim()} loading={run.isPending} onClick={() => run.mutate()}>
            {t('导入', 'Import')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}

/**
 * 导出成文本，一行一条完整 URL（含账号密码——导出就是为了拿去别处用，打了码的地址用不了）。
 * 勾选名称时写成「URL 名称」，正好是批量导入认的格式，能原样导回来。
 *
 * 在前端拼而不是走后端下载接口：列表里已经是完整 URL，而管理接口要带登录头，
 * 一个裸 `<a href>` 下载会吃 401。
 */
function ExportProxiesDialog({
  open,
  onOpenChange,
  proxies,
  scoped,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  proxies: SavedProxy[]
  /** 是否只导出勾选的那些（否则是整个池子）。 */
  scoped: boolean
}) {
  const { t } = useI18n()
  const [withNames, setWithNames] = useState(false)
  const text = proxies.map((p) => (withNames ? `${p.url} ${p.label}` : p.url)).join('\n') + '\n'

  const download = () => {
    const blob = new Blob([text], { type: 'text/plain;charset=utf-8' })
    const href = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = href
    a.download = `coban-proxies-${new Date().toISOString().slice(0, 10)}.txt`
    a.click()
    URL.revokeObjectURL(href)
    onOpenChange(false)
  }
  const copy = async () => {
    const ok = await copyText(text)
    toastManager.add({
      title: ok ? t(`已复制 ${proxies.length} 条`, `Copied ${proxies.length}`) : t('复制失败', 'Copy failed'),
      type: ok ? 'success' : 'error',
    })
    if (ok) onOpenChange(false)
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-lg">
        <DialogHeader>
          <DialogTitle>
            {scoped
              ? t(`导出选中的 ${proxies.length} 条代理`, `Export ${proxies.length} selected prox${proxies.length === 1 ? 'y' : 'ies'}`)
              : t(`导出全部 ${proxies.length} 条代理`, `Export all ${proxies.length} prox${proxies.length === 1 ? 'y' : 'ies'}`)}
          </DialogTitle>
          <DialogDescription>
            {t(
              '一行一条完整地址，包含账号密码，注意保管。',
              'One full URL per line, including credentials — keep the file safe.',
            )}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel className="space-y-3">
          <label className="flex items-center justify-between gap-3 text-sm">
            <span>
              {t('附带名称', 'Include names')}
              <span className="block text-xs text-muted-foreground">
                {t('写成「地址 名称」，可原样批量导入回来', 'Written as “URL name”; re-importable as is')}
              </span>
            </span>
            <Switch checked={withNames} onCheckedChange={setWithNames} />
          </label>
          <pre className="max-h-40 overflow-auto rounded-md border bg-muted/40 p-2 font-mono text-xs">
            {proxies.slice(0, 5).map((p) => (withNames ? `${proxyMaskedUrl(p.url)} ${p.label}` : proxyMaskedUrl(p.url))).join('\n')}
            {proxies.length > 5 && `\n… ${t(`共 ${proxies.length} 行`, `${proxies.length} lines`)}`}
          </pre>
        </DialogPanel>
        <DialogFooter>
          <Button variant="outline" onClick={copy}>
            <CopyIcon />
            {t('复制', 'Copy')}
          </Button>
          <Button onClick={download}>
            <DownloadIcon />
            {t('下载 .txt', 'Download .txt')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
