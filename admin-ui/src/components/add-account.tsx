import { useEffect, useRef, useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { ArrowRightIcon, CopyIcon, ExternalLinkIcon, FileJsonIcon, UploadIcon } from 'lucide-react'
import { exchangeCode, getAuthorizeUrl, importAuthJson, type ImportReport } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { copyText, displayCredentialLabel, extractError } from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import {
  Dialog, DialogClose, DialogDescription, DialogFooter, DialogHeader,
  DialogPanel, DialogPopup, DialogTitle,
} from '@/components/ui/dialog'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Tabs, TabsList, TabsPanel, TabsTab } from '@/components/ui/tabs'
import { Textarea } from '@/components/ui/textarea'
import { toastManager } from '@/components/ui/toast'

interface AuthorizeRequest {
  session: number
  popup: Window | null
}

/** 读进来的文件的显示信息。`accounts` 为 null 表示这份内容解析不出来。 */
interface LoadedFile {
  name: string
  size: string
  accounts: number | null
}

/**
 * 选文件导入的大小上限。
 *
 * 凭证文件是几 KB 到几百 KB 的量级（23 个账号的批量导出约 150 KB），8 MB 已经宽出两个
 * 数量级。设这道闸不是防攻击——文件是自己选的——而是防手滑选错：把一个几百 MB 的日志
 * 拖进来，浏览器会先卡住再把它塞进 textarea，那时候页面基本没救了。
 */
const MAX_IMPORT_BYTES = 8 * 1024 * 1024

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

/**
 * 本地数一下这份内容里有几个账号，解析不出来返回 null。
 *
 * 与后端 `import_auth_json` 的判定保持一致：根上有 `accounts` 数组就按批量算，否则整份
 * 当一个账号。这里只为在界面上先给个数，真正的取舍仍在服务端。
 */
function countAccounts(text: string): number | null {
  try {
    const v = JSON.parse(text)
    if (v && typeof v === 'object' && Array.isArray((v as { accounts?: unknown[] }).accounts)) {
      return (v as { accounts: unknown[] }).accounts.length
    }
    return 1
  } catch {
    return null
  }
}

/**
 * 添加账号弹窗。两条路：
 *
 * - **浏览器授权**：打开 OpenAI 授权页，完成后浏览器会跳到 `localhost:1455/auth/callback`
 *   ——那个地址是 codex CLI 本机监听的，coban 这边**连不上是正常的**，页面报错也没关系，
 *   地址栏里那条 URL 就是全部所需。这一点必须在界面上说清楚，否则用户看到「无法访问此
 *   网站」会以为授权失败了，然后从头再来一遍。
 * - **导入凭证**：这台机器已经 `codex login` 过的话，直接把 `~/.codex/auth.json` 贴进来；
 *   带 `accounts` 数组的批量导出也认，逐个导入、坏的跳过。服务器上没有图形界面时，
 *   这常常是唯一走得通的路。
 */
export function AddAccount({
  open, onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const [authUrl, setAuthUrl] = useState<string | null>(null)
  const [authState, setAuthState] = useState<string | null>(null)
  const [callback, setCallback] = useState('')
  const [authJson, setAuthJson] = useState('')
  const [loaded, setLoaded] = useState<LoadedFile | null>(null)
  const [dragging, setDragging] = useState(false)
  const authorizeSession = useRef(0)
  const fileInput = useRef<HTMLInputElement>(null)
  /**
   * 读进来的文件内容。
   *
   * **刻意不放进那个文本框**：一份 23 账号的导出有 150 KB，而文本框是
   * `field-sizing-content`（跟着内容长高、没有上限），灌进去会把弹窗撑到几十屏；更要紧的是
   * 那里面是 access_token / refresh_token 明文，摊在屏幕上等着被截图或录屏。
   * 读进来只报文件名、大小和账号数，内容本身只在提交时用。
   */
  const [fileContent, setFileContent] = useState<string | null>(null)
  /** 待提交的内容：读了文件用文件的，否则用手填的。 */
  const content = fileContent ?? authJson

  const reset = () => {
    setCallback('')
    setAuthJson('')
    setFileContent(null)
    setLoaded(null)
    setDragging(false)
    setAuthUrl(null)
    setAuthState(null)
  }
  const clearFile = () => {
    setFileContent(null)
    setLoaded(null)
  }

  /**
   * 读一个本地文件备着提交。
   *
   * **只在浏览器里读**，服务端拿到的还是同一个 `content` 字符串——选文件只是省掉「打开
   * 文件、全选、复制」这三步，没有新增一条把凭证送出去的路径。
   *
   * 读完顺手本地解析一次，把账号数报出来：选错文件（比如 package.json）时当场就能看出来，
   * 不必先提交再等后端报错。解析失败也照样收下，只是摘要那行改口说「不是合法 JSON」——
   * 判定权仍在服务端，前端抢着拒会把「后端认得、前端不认」的形态挡在门外。
   */
  const loadFile = async (file: File) => {
    if (file.size > MAX_IMPORT_BYTES) {
      toastManager.add({
        title: t('文件太大', 'File too large'),
        description: t(
          `${formatBytes(file.size)}，上限 ${formatBytes(MAX_IMPORT_BYTES)}。凭证文件不该有这么大，确认一下选对了吗。`,
          `${formatBytes(file.size)} exceeds the ${formatBytes(MAX_IMPORT_BYTES)} limit. A credentials file should never be this large — check you picked the right one.`,
        ),
        type: 'error',
      })
      return
    }
    let text: string
    try {
      text = await file.text()
    } catch (error) {
      toastManager.add({
        title: t('读取文件失败', 'Could not read the file'),
        description: extractError(error, language),
        type: 'error',
      })
      return
    }
    setFileContent(text)
    setLoaded({ name: file.name, size: formatBytes(file.size), accounts: countAccounts(text) })
  }
  const handleOpenChange = (next: boolean) => {
    if (!next) reset()
    onOpenChange(next)
  }

  useEffect(() => {
    // 每次开关都让上一轮的授权请求作废：慢响应回来时弹窗可能已经关了又开，
    // 那时把旧的 URL 填进新的一轮就会用一个已经作废的 state 去换 token。
    authorizeSession.current += 1
    if (open) reset()
  }, [open])

  const added = (label: string) => {
    toastManager.add({
      title: t('已添加账号', 'Account added'),
      description: displayCredentialLabel(label, language),
      type: 'success',
    })
    void qc.invalidateQueries({ queryKey: ['credentials'] })
    handleOpenChange(false)
  }
  /**
   * 批量导入的结果提示。
   *
   * 有跳过的就用 warning 而不是 success：一次「22/23」不该和「23/23」长得一样。
   * 原因最多列三条——再多也看不完，剩下的落在服务端日志里。
   */
  const addedMany = (report: ImportReport) => {
    const { imported, skipped } = report
    const lines = skipped.slice(0, 3).map((s) => `${s.name}: ${s.reason}`)
    if (skipped.length > lines.length) {
      lines.push(t(`…另有 ${skipped.length - lines.length} 个`, `…and ${skipped.length - lines.length} more`))
    }
    toastManager.add({
      title: skipped.length
        ? t(`已导入 ${imported.length} 个，跳过 ${skipped.length} 个`,
            `Imported ${imported.length}, skipped ${skipped.length}`)
        : t(`已导入 ${imported.length} 个账号`, `Imported ${imported.length} accounts`),
      description: lines.join('\n') || undefined,
      type: skipped.length ? 'warning' : 'success',
    })
    void qc.invalidateQueries({ queryKey: ['credentials'] })
    handleOpenChange(false)
  }
  const failed = (title: string) => (error: unknown) => toastManager.add({
    title,
    description: extractError(error, language),
    type: 'error',
  })

  const authorize = useMutation({
    mutationFn: (_request: AuthorizeRequest) => getAuthorizeUrl(),
    onSuccess: ({ url, state }, request) => {
      if (request.session !== authorizeSession.current) {
        request.popup?.close()
        return
      }
      setAuthUrl(url)
      setAuthState(state)
      if (request.popup && !request.popup.closed) {
        try {
          request.popup.location.replace(url)
        } catch {
          // 跨窗口导航失败时，弹窗内仍会显示可手动点击的授权链接。
        }
      }
    },
    onError: (error, request) => {
      request.popup?.close()
      if (request.session !== authorizeSession.current) return
      failed(t('生成授权链接失败', 'Failed to create authorization link'))(error)
    },
  })

  const exchange = useMutation({
    mutationFn: () => exchangeCode(callback.trim(), authState ?? undefined),
    onSuccess: (cred) => added(cred.label),
    onError: failed(t('添加失败', 'Failed to add account')),
  })

  const importJson = useMutation({
    mutationFn: () => importAuthJson(content.trim()),
    // 单个账号沿用「已添加账号 + 标签」那条提示；批量则报数，并把跳过的原因一并列出
    // ——只报「导入 22 个」会让人以为 23 个里那一个是自己数错了。
    onSuccess: (report) => {
      if (report.imported.length === 1 && report.skipped.length === 0) {
        added(report.imported[0].label)
        return
      }
      addedMany(report)
    },
    onError: failed(t('导入失败', 'Import failed')),
  })

  const busy = authorize.isPending || exchange.isPending || importJson.isPending

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && busy) return
        handleOpenChange(next)
      }}
    >
      <DialogPopup closeProps={{ disabled: busy }} className="max-w-xl">
        <DialogHeader>
          <DialogTitle>{t('添加 ChatGPT 账号', 'Add ChatGPT account')}</DialogTitle>
          <DialogDescription>
            {t(
              '用 ChatGPT 订阅账号授权，或直接导入这台机器上已登录的 codex 凭证。',
              'Authorize with a ChatGPT subscription account, or import credentials this machine has already signed in with.',
            )}
          </DialogDescription>
        </DialogHeader>

        <Tabs defaultValue="oauth">
          <DialogPanel className="space-y-4">
            <TabsList aria-label={t('添加方式', 'How to add')}>
              <TabsTab value="oauth">{t('浏览器授权', 'Browser authorization')}</TabsTab>
              <TabsTab value="import">{t('导入凭证', 'Import credentials')}</TabsTab>
            </TabsList>

            <TabsPanel value="oauth" className="space-y-5">
              <Field>
                <FieldLabel>{t('1. 打开授权页面', '1. Open the authorization page')}</FieldLabel>
                <FieldDescription>
                  {t(
                    '用要接入的 ChatGPT 订阅账号完成授权。',
                    'Authorize with the ChatGPT subscription account you want to connect.',
                  )}
                </FieldDescription>
                <Button
                  type="button"
                  variant="outline"
                  loading={authorize.isPending}
                  onClick={() => {
                    const popup = window.open('about:blank', '_blank')
                    if (popup) popup.opener = null
                    authorize.mutate({ session: authorizeSession.current, popup })
                  }}
                >
                  <ExternalLinkIcon />
                  {t('打开 OpenAI 授权页', 'Open the OpenAI authorization page')}
                </Button>
              </Field>

              {authUrl && (
                <Alert variant="info">
                  <ExternalLinkIcon aria-hidden />
                  <AlertTitle>{t('授权页已在新标签打开', 'Authorization page opened in a new tab')}</AlertTitle>
                  <AlertDescription>
                    <p>
                      {t('如果浏览器拦截了新标签页，可', 'If your browser blocked the new tab, you can')}{' '}
                      <a href={authUrl} target="_blank" rel="noopener">
                        {t('手动打开授权页面', 'open the authorization page manually')}
                      </a>
                      {t(
                        '，或把链接复制到其它浏览器/设备上完成授权。',
                        ', or copy the link to another browser or device to finish authorization.',
                      )}
                    </p>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      className="mt-2"
                      onClick={async () => {
                        const copied = await copyText(authUrl)
                        toastManager.add(copied
                          ? { title: t('已复制授权链接', 'Authorization link copied'), type: 'success' }
                          : {
                              title: t('复制失败，请手动复制', 'Copy failed; copy the link manually'),
                              description: authUrl,
                              type: 'error',
                            })
                      }}
                    >
                      <CopyIcon />
                      {t('复制授权链接', 'Copy authorization link')}
                    </Button>
                  </AlertDescription>
                </Alert>
              )}

              {/*
                这条提示是这个流程里最容易劝退的一步：授权完成后浏览器会跳到 codex CLI 才
                监听的 localhost:1455，多半直接显示「无法访问此网站」。不说清楚的话，
                用户会判定授权失败并重来一遍，而正确做法只是复制地址栏。
              */}
              <Alert variant="warning">
                <AlertTitle>{t('页面打不开是正常的', 'That error page is expected')}</AlertTitle>
                <AlertDescription>
                  {t(
                    '授权完成后浏览器会跳到 http://localhost:1455/auth/callback?code=… ，这个地址由 codex 命令行监听，这里打不开。直接复制地址栏里那条完整 URL 粘到下面即可。',
                    'After authorizing, the browser jumps to http://localhost:1455/auth/callback?code=… — that address belongs to the codex CLI and will not load here. Just copy the full URL from the address bar and paste it below.',
                  )}
                </AlertDescription>
              </Alert>

              <Form
                onSubmit={(event) => {
                  event.preventDefault()
                  if (!exchange.isPending && callback.trim()) exchange.mutate()
                }}
                className="space-y-4"
              >
                <Field name="callback">
                  <FieldLabel htmlFor="oauth-callback">
                    {t('2. 粘贴回调地址', '2. Paste the callback URL')}
                  </FieldLabel>
                  <Textarea
                    id="oauth-callback"
                    name="callback"
                    value={callback}
                    onChange={(event) => setCallback(event.target.value)}
                    placeholder="http://localhost:1455/auth/callback?code=…&state=…"
                    className="min-h-24 font-mono text-xs"
                    required
                  />
                  <FieldDescription>
                    {t(
                      '整条 URL 或只有 code 的那一段都可以。',
                      'Either the whole URL or just the code fragment works.',
                    )}
                  </FieldDescription>
                </Field>
                <Button type="submit" loading={exchange.isPending} disabled={!callback.trim()}>
                  <ArrowRightIcon />
                  {t('添加账号', 'Add account')}
                </Button>
              </Form>
            </TabsPanel>

            <TabsPanel value="import" className="space-y-4">
              <Alert>
                <FileJsonIcon aria-hidden />
                <AlertDescription>
                  {t(
                    '把 ~/.codex/auth.json 的内容整段贴进来；带 accounts 数组的批量导出也认，会逐个导入。这些内容含 refresh token，只在你自己的机器之间传。',
                    'Paste the whole contents of ~/.codex/auth.json. A batch export with an accounts array also works — each account is imported separately. This content contains refresh tokens — only move it between machines you own.',
                  )}
                </AlertDescription>
              </Alert>
              <Form
                onSubmit={(event) => {
                  event.preventDefault()
                  if (!importJson.isPending && content.trim()) importJson.mutate()
                }}
                className="space-y-4"
              >
                <input
                  ref={fileInput}
                  type="file"
                  accept=".json,application/json"
                  className="hidden"
                  onChange={(event) => {
                    const file = event.target.files?.[0]
                    // 选同一个文件两次时 change 不会再触发，除非把 value 清掉。
                    event.target.value = ''
                    if (file) void loadFile(file)
                  }}
                />
                <div
                  onDragOver={(event) => {
                    event.preventDefault()
                    if (!dragging) setDragging(true)
                  }}
                  onDragLeave={() => setDragging(false)}
                  onDrop={(event) => {
                    event.preventDefault()
                    setDragging(false)
                    const file = event.dataTransfer.files?.[0]
                    if (file) void loadFile(file)
                  }}
                  className={`flex flex-col items-center gap-2 rounded-lg border border-dashed px-4 py-6 text-center transition-colors ${
                    dragging ? 'border-primary bg-primary/5' : 'border-input'
                  }`}
                >
                  <UploadIcon className="size-5 text-muted-foreground" aria-hidden />
                  <p className="text-sm text-muted-foreground">
                    {t('把 .json 文件拖到这里', 'Drop a .json file here')}
                  </p>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => fileInput.current?.click()}
                  >
                    {t('选择文件…', 'Choose file…')}
                  </Button>
                </div>

                {/* 读了文件就只给一行摘要，不回显内容——见 fileContent 的注。 */}
                {loaded ? (
                  <div className="flex items-center gap-3 rounded-lg border bg-muted/40 px-3 py-2.5">
                    <FileJsonIcon className="size-4 shrink-0 text-muted-foreground" aria-hidden />
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-sm font-medium">{loaded.name}</p>
                      <p className="text-xs text-muted-foreground">
                        {loaded.accounts === null
                          ? t(
                              `${loaded.size} · 内容不是合法 JSON，提交后由服务端判定`,
                              `${loaded.size} · not valid JSON — the server will decide`,
                            )
                          : t(
                              `${loaded.size} · 解析到 ${loaded.accounts} 个账号`,
                              `${loaded.size} · ${loaded.accounts} account(s) found`,
                            )}
                      </p>
                    </div>
                    <Button type="button" variant="ghost" size="sm" onClick={clearFile}>
                      {t('移除', 'Remove')}
                    </Button>
                  </div>
                ) : (
                  <Field name="auth-json">
                    <FieldLabel htmlFor="auth-json">{t('或直接粘贴', 'Or paste directly')}</FieldLabel>
                    <Textarea
                      id="auth-json"
                      name="auth-json"
                      value={authJson}
                      onChange={(event) => setAuthJson(event.target.value)}
                      placeholder={'{\n  "tokens": { "access_token": "…", "refresh_token": "…" }\n}\n\n{\n  "accounts": [{ "credentials": { "access_token": "…", "refresh_token": "…" } }]\n}'}
                      // 文本框是 field-sizing-content（跟着内容长高），不封顶的话粘一份
                      // 大导出同样会把弹窗撑爆——封个高度让它内部滚。
                      className="max-h-56 min-h-40 overflow-y-auto font-mono text-xs"
                    />
                  </Field>
                )}
                <Button type="submit" loading={importJson.isPending} disabled={!content.trim()}>
                  <ArrowRightIcon />
                  {t('导入', 'Import')}
                </Button>
              </Form>
            </TabsPanel>
          </DialogPanel>
        </Tabs>

        <DialogFooter>
          <DialogClose render={<Button variant="ghost" />} disabled={busy}>
            {t('关闭', 'Close')}
          </DialogClose>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
