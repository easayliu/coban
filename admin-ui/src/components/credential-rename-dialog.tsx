import { useEffect, useState } from 'react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { displayCredentialLabel } from '@/lib/utils'
import { type CredentialActions } from '@/components/credential-shared'
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
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'

/**
 * 改备注。**表格视图专用**——卡片那边是标题上的内联编辑（行里没有内联编辑位，列宽是写死的）。
 *
 * 原来表格走的是一次 `window.prompt`：桌面上已经不能切到卡片视图了（≥80rem 只有表格，见
 * credential-workspace 的 [LIST_VIEW_MEDIA]），于是那个原生弹框成了宽屏唯一的改名路径。它不受
 * 主题与语言管、没法说明这个名字是干什么用的、在部分浏览器里还会被当成弹窗拦掉，不该是唯一入口。
 */
export function CredentialRenameDialog({
  cred,
  open,
  onOpenChange,
  rename,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  rename: CredentialActions['rename']
}) {
  const { t, language } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [name, setName] = useState(cred.label)

  // 每次打开都回到服务端那份：上次改了一半没保存就关掉的残留留到下次，会让人以为已经生效。
  useEffect(() => {
    if (open) setName(cred.label)
  }, [open, cred.label])

  const next = name.trim()
  const dirty = next.length > 0 && next !== cred.label
  const save = () => {
    if (!dirty) return
    rename.mutate(next, { onSuccess: () => onOpenChange(false) })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-md">
        <DialogHeader>
          <DialogTitle>{t('账号备注', 'Account label')}</DialogTitle>
          {/* 弹窗标题写的是「改什么」，这里写「改哪一个」——一屏几十行，光看标题认不出是哪个号。 */}
          <DialogDescription className="mt-1 truncate" title={credentialLabel}>
            {credentialLabel}
          </DialogDescription>
        </DialogHeader>

        <DialogPanel>
          <Form
            onSubmit={(event) => {
              event.preventDefault()
              save()
            }}
          >
            <Field>
              <FieldLabel>{t('备注', 'Label')}</FieldLabel>
              <Input
                value={name}
                onChange={(event) => setName(event.target.value)}
                autoFocus
                aria-label={t('账号备注', 'Account label')}
              />
              <FieldDescription>
                {t(
                  '只是这个控制台里的显示名，不影响上游账号本身。留空会被忽略。',
                  'Display name inside this console only; it does not touch the upstream account. Blank input is ignored.',
                )}
              </FieldDescription>
            </Field>
          </Form>
        </DialogPanel>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button onClick={save} disabled={!dirty || rename.isPending} loading={rename.isPending}>
            {t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
