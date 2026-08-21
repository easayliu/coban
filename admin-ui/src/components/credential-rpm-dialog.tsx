import { useEffect, useState } from 'react'
import { GaugeIcon } from 'lucide-react'
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
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import {
  NumberField, NumberFieldDecrement, NumberFieldGroup, NumberFieldIncrement, NumberFieldInput,
} from '@/components/ui/number-field'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'

/** 三态策略；与后端的 `rpm_limit` 取值一一对应（0 / -1 / 正数）。 */
type RpmPolicy = 'default' | 'unlimited' | 'custom'

const POLICY_ITEMS = [
  { value: 'default', chinese: '跟随默认', english: 'Use default' },
  { value: 'unlimited', chinese: '不限', english: 'Unlimited' },
  { value: 'custom', chinese: '独立上限', english: 'Custom limit' },
] as const

function policyFromLimit(limit: number): RpmPolicy {
  if (limit === 0) return 'default'
  if (limit < 0) return 'unlimited'
  return 'custom'
}

/**
 * 逐账号 RPM 上限的编辑框：这个号最近 60 秒最多转发多少条请求。
 *
 * 口径与列表里那列「RPM」完全一致（同一个 60 秒窗口、同样含失败与 count_tokens），
 * 所以两个数可以直接比着看。三态与设备上限对齐：跟随全局默认 / 明确不限 / 独立上限。
 */
export function CredentialRpmDialog({
  cred,
  open,
  onOpenChange,
  rpmLimit,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  rpmLimit: CredentialActions['rpmLimit']
}) {
  const { t, language, locale } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [policy, setPolicy] = useState<RpmPolicy>(() => policyFromLimit(cred.rpm_limit))
  // 自定义值的初值：本来就是独立上限就沿用它，否则拿生效值起步（多半就是想在它附近调），
  // 再兜底一个 60。
  const [custom, setCustom] = useState(() =>
    Math.max(1, cred.rpm_limit > 0 ? cred.rpm_limit : cred.rpm_limit_effective || 60))

  // 每次打开都从服务端那份重置：上次改了一半没保存就关掉的残留留到下次，会让人以为它已经生效。
  useEffect(() => {
    if (!open) return
    setPolicy(policyFromLimit(cred.rpm_limit))
    setCustom(Math.max(1, cred.rpm_limit > 0 ? cred.rpm_limit : cred.rpm_limit_effective || 60))
  }, [open, cred.rpm_limit, cred.rpm_limit_effective])

  const policyItems = POLICY_ITEMS.map((item) => ({
    value: item.value,
    label: t(item.chinese, item.english),
  }))
  const next = policy === 'default' ? 0 : policy === 'unlimited' ? -1 : Math.max(1, Math.floor(custom))
  const dirty = next !== cred.rpm_limit
  const effective = cred.rpm_limit_effective > 0
    ? t(
      `${cred.rpm_limit_effective.toLocaleString(locale)} 条 / 分钟`,
      `${cred.rpm_limit_effective.toLocaleString(locale)} req/min`,
    )
    : t('不限', 'Unlimited')

  const save = () => rpmLimit.mutate(next, { onSuccess: () => onOpenChange(false) })

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t('RPM 上限', 'RPM limit')}</DialogTitle>
          <DialogDescription className="mt-1 truncate" title={credentialLabel}>
            {credentialLabel}
          </DialogDescription>
        </DialogHeader>

        <DialogPanel className="space-y-4">
          <div className="flex items-baseline justify-between gap-2 rounded-lg border px-3 py-2">
            <span className="text-muted-foreground text-xs">{t('当前 / 生效上限', 'Current / effective limit')}</span>
            <span className="font-medium text-sm tabular-nums">
              {cred.rpm.toLocaleString(locale)}
              <span className="text-muted-foreground">
                {' / '}
                {cred.rpm_limit_effective > 0 ? cred.rpm_limit_effective.toLocaleString(locale) : '∞'}
              </span>
            </span>
          </div>

          <div className="grid gap-4 sm:grid-cols-2">
            <Field>
              <FieldLabel>{t('上限策略', 'Limit policy')}</FieldLabel>
              <Select
                items={policyItems}
                value={policy}
                onValueChange={(value) => { if (value) setPolicy(value as RpmPolicy) }}
              >
                <SelectTrigger aria-label={t('上限策略', 'Limit policy')}>
                  <SelectValue />
                </SelectTrigger>
                <SelectPopup>
                  {policyItems.map((item) => (
                    <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              <FieldDescription>
                {t(
                  `“跟随默认”套用全局设置，当前为 ${effective}。`,
                  `“Use default” applies the global setting, currently ${effective}.`,
                )}
              </FieldDescription>
            </Field>

            {policy === 'custom' && (
              <Field>
                <FieldLabel>{t('每分钟最多请求数', 'Maximum requests per minute')}</FieldLabel>
                <NumberField
                  value={custom}
                  min={1}
                  step={1}
                  onValueChange={(value) => setCustom(Math.max(1, Math.floor(value ?? 1)))}
                >
                  <NumberFieldGroup>
                    <NumberFieldDecrement />
                    <NumberFieldInput aria-label={t('自定义 RPM 上限', 'Custom RPM limit')} />
                    <NumberFieldIncrement />
                  </NumberFieldGroup>
                </NumberField>
                <FieldDescription>
                  {t('该设置只影响当前账号。', 'This setting only affects the current account.')}
                </FieldDescription>
              </Field>
            )}
          </div>

          <Alert>
            <GaugeIcon />
            <AlertDescription>
              {t(
                '打满之后这个号暂时退出选号，请求自动分流到别的账号；所有号都打满时客户端收到 429 与 retry-after。计数在服务端内存里，重启即清零。',
                'Once full, this account temporarily drops out of selection and requests spill over to another one; when every account is full the caller gets a 429 with retry-after. Counting lives in server memory and resets on restart.',
              )}
            </AlertDescription>
          </Alert>
        </DialogPanel>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button onClick={save} disabled={!dirty || rpmLimit.isPending} loading={rpmLimit.isPending}>
            {t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
