import { useEffect, useRef, useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { ArrowRightIcon, CopyIcon, ExternalLinkIcon, FileJsonIcon } from 'lucide-react'
import { exchangeCode, getAuthorizeUrl, importAuthJson } from '@/api/credentials'
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

/**
 * 添加账号弹窗。两条路：
 *
 * - **浏览器授权**：打开 OpenAI 授权页，完成后浏览器会跳到 `localhost:1455/auth/callback`
 *   ——那个地址是 codex CLI 本机监听的，coban 这边**连不上是正常的**，页面报错也没关系，
 *   地址栏里那条 URL 就是全部所需。这一点必须在界面上说清楚，否则用户看到「无法访问此
 *   网站」会以为授权失败了，然后从头再来一遍。
 * - **导入 auth.json**：这台机器已经 `codex login` 过的话，直接把文件内容贴进来。
 *   服务器上没有图形界面时，这常常是唯一走得通的路。
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
  const authorizeSession = useRef(0)

  const reset = () => {
    setCallback('')
    setAuthJson('')
    setAuthUrl(null)
    setAuthState(null)
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
    mutationFn: () => importAuthJson(authJson.trim()),
    onSuccess: (cred) => added(cred.label),
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
              <TabsTab value="import">{t('导入 auth.json', 'Import auth.json')}</TabsTab>
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
                    '把 ~/.codex/auth.json 的内容整段贴进来。这个文件含 refresh token，只在你自己的机器之间传。',
                    'Paste the whole contents of ~/.codex/auth.json. It contains a refresh token — only move it between machines you own.',
                  )}
                </AlertDescription>
              </Alert>
              <Form
                onSubmit={(event) => {
                  event.preventDefault()
                  if (!importJson.isPending && authJson.trim()) importJson.mutate()
                }}
                className="space-y-4"
              >
                <Field name="auth-json">
                  <FieldLabel htmlFor="auth-json">auth.json</FieldLabel>
                  <Textarea
                    id="auth-json"
                    name="auth-json"
                    value={authJson}
                    onChange={(event) => setAuthJson(event.target.value)}
                    placeholder={'{\n  "tokens": { "access_token": "…", "refresh_token": "…" }\n}'}
                    className="min-h-40 font-mono text-xs"
                    required
                  />
                </Field>
                <Button type="submit" loading={importJson.isPending} disabled={!authJson.trim()}>
                  <ArrowRightIcon />
                  {t('导入账号', 'Import account')}
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
