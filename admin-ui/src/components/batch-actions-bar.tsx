import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { ChevronDownIcon, PauseIcon, PlayIcon, Trash2Icon, XIcon } from 'lucide-react'
import {
  deleteCredentials, setDisabledMany, setPriorities, setRpmLimits,
  type Credential,
} from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { cn, extractError } from '@/lib/utils'
import {
  AlertDialog, AlertDialogClose, AlertDialogDescription, AlertDialogFooter,
  AlertDialogHeader, AlertDialogPopup, AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import {
  Card,
} from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import {
  NumberField, NumberFieldDecrement, NumberFieldGroup, NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { toastManager } from '@/components/ui/toast'
import { Toolbar } from '@/components/ui/toolbar'

const RPM_MODE_ITEMS = [
  { value: 'default', chinese: '跟随默认', english: 'Use default' },
  { value: 'unlimited', chinese: '不限速', english: 'Unlimited' },
  { value: 'custom', chinese: '独立上限', english: 'Custom limit' },
] as const

/**
 * 批量操作条：全选/清空 + 优先级 / RPM 上限 / 启停 / 删除。
 *
 * 所有操作都作用于**当前筛选结果里被勾选的账号**（跨页保留勾选）。写操作走各自的批量
 * 接口，后端在单事务内完成，不会出现「改了一半」的中间态。
 */
export function BatchActionsBar({
  all, selected, onSelectedChange, onClear,
}: {
  all: Credential[]
  selected: Set<number>
  onSelectedChange: (next: Set<number>) => void
  onClear: () => void
}) {
  const { t, language, locale } = useI18n()
  const qc = useQueryClient()
  const [priority, setPriority] = useState(0)
  const [rpmMode, setRpmMode] = useState<'default' | 'unlimited' | 'custom'>('default')
  const [customRpm, setCustomRpm] = useState(60)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [advancedOpen, setAdvancedOpen] = useState(false)

  const ids = [...selected]
  const n = selected.size
  const formattedCount = n.toLocaleString(locale)
  const formattedTotal = all.length.toLocaleString(locale)
  const englishAccountCount = `${formattedCount} ${n === 1 ? 'account' : 'accounts'}`
  const rpmModeItems = RPM_MODE_ITEMS.map((item) => ({
    value: item.value,
    label: t(item.chinese, item.english),
  }))
  const notify = (msg: string, clearSelection = false) => {
    toastManager.add({ title: msg, type: 'success' })
    qc.invalidateQueries({ queryKey: ['credentials'] })
    if (clearSelection) onSelectedChange(new Set())
  }
  const onError = (error: unknown) => toastManager.add({
    title: t('批量操作失败', 'Batch operation failed'),
    description: extractError(error, language),
    type: 'error',
  })

  const applyPriority = useMutation({
    mutationFn: (p: number) => setPriorities(ids, p),
    onSuccess: (_r, p) => notify(t(
      `已把 ${formattedCount} 个账号设为 P${p}`,
      `Set ${englishAccountCount} to P${p}`,
    )),
    onError,
  })
  const applyRpmLimit = useMutation({
    mutationFn: (v: number) => setRpmLimits(ids, v),
    onSuccess: (_r, v) =>
      notify(
        v > 0 ? t(
          `已把 ${formattedCount} 个账号的 RPM 上限设为 ${v.toLocaleString(locale)}`,
          `Set the RPM limit for ${englishAccountCount} to ${v.toLocaleString(locale)}`,
        )
          : v === 0 ? t(
            `已把 ${formattedCount} 个账号改为跟随全局默认 RPM 上限`,
            `Set ${englishAccountCount} to use the global default RPM limit`,
          )
            : t(
              `已把 ${formattedCount} 个账号设为不限 RPM`,
              `Set ${englishAccountCount} to unlimited RPM`,
            ),
      ),
    onError,
  })
  const applyDisabled = useMutation({
    mutationFn: (d: boolean) => setDisabledMany(ids, d),
    onSuccess: (_r, d) => notify(t(
      `已${d ? '停用' : '启用'} ${formattedCount} 个账号`,
      `${d ? 'Disabled' : 'Enabled'} ${englishAccountCount}`,
    )),
    onError,
  })
  const applyDelete = useMutation({
    mutationFn: () => deleteCredentials(ids),
    // 账号已不存在，留着勾选没有意义，顺手清空。批量条不会随之卸载，确认框得自己关。
    onSuccess: () => {
      setConfirmDelete(false)
      notify(t(`已删除 ${formattedCount} 个账号`, `Deleted ${englishAccountCount}`), true)
    },
    onError: (e) => { setConfirmDelete(false); onError(e) },
  })

  const busy =
    applyPriority.isPending || applyRpmLimit.isPending ||
    applyDisabled.isPending || applyDelete.isPending
  const allSelected = all.length > 0 && all.every((item) => selected.has(item.id))
  const rpmLimit = rpmMode === 'default' ? 0 : rpmMode === 'unlimited' ? -1 : Math.max(1, Math.floor(customRpm))

  return (
    <Card render={<section aria-label={t('批量操作', 'Batch actions')} />} className="rounded-xl">
        <div className="flex min-h-14 flex-wrap items-center gap-3 p-3">
          <label className="mr-auto flex cursor-pointer items-center gap-2 text-xs">
            <Checkbox
              checked={allSelected}
              indeterminate={n > 0 && !allSelected}
              onCheckedChange={(checked) => onSelectedChange(checked ? new Set(all.map((item) => item.id)) : new Set())}
            />
            <span aria-live="polite">
              {t('已选', 'Selected')}{' '}
              <span className="tnum font-semibold text-foreground">{formattedCount}</span>
              <span className="text-muted-foreground"> / {formattedTotal}</span>
            </span>
          </label>

          <Toolbar className="gap-3 border-0 bg-transparent p-0 shadow-none">
            <Button size="sm" variant="outline" aria-label={t('启用所选账号', 'Enable selected accounts')} disabled={busy} loading={applyDisabled.isPending && applyDisabled.variables === false} onClick={() => applyDisabled.mutate(false)}>
              <PlayIcon /><span className="max-sm:sr-only">{t('启用', 'Enable')}</span>
            </Button>
            <Button size="sm" variant="outline" aria-label={t('停用所选账号', 'Disable selected accounts')} disabled={busy} loading={applyDisabled.isPending && applyDisabled.variables === true} onClick={() => applyDisabled.mutate(true)}>
              <PauseIcon /><span className="max-sm:sr-only">{t('停用', 'Disable')}</span>
            </Button>
            <Button size="sm" variant="destructive-outline" aria-label={t('删除所选账号', 'Delete selected accounts')} disabled={busy} onClick={() => setConfirmDelete(true)}>
              <Trash2Icon /><span className="max-sm:sr-only">{t('删除', 'Delete')}</span>
            </Button>
          </Toolbar>

          <Button
            size="sm"
            variant={advancedOpen ? 'secondary' : 'outline'}
            aria-expanded={advancedOpen}
            aria-controls="batch-advanced-settings"
            onClick={() => setAdvancedOpen((open) => !open)}
          >
            {t('更多设置', 'More settings')}
            <ChevronDownIcon className={cn('size-4 transition-transform', advancedOpen && 'rotate-180')} />
          </Button>

          <Button size="icon-sm" variant="ghost" onClick={onClear} title={t('清空选择', 'Clear selection')} aria-label={t('清空选择', 'Clear selection')}>
            <XIcon />
          </Button>
        </div>

        {advancedOpen && (
          <div id="batch-advanced-settings" className="grid gap-3 border-t p-3 lg:grid-cols-2">
            <div className="flex flex-wrap items-center gap-2">
              <div className="mr-auto min-w-28">
                <div className="text-xs font-medium">{t('调度优先级', 'Scheduling priority')}</div>
                <div className="text-xs text-muted-foreground">{t('数值越小越优先', 'Lower values have higher priority')}</div>
              </div>
              <NumberField
                id="batch-priority"
                value={priority}
                min={0}
                step={1}
                size="sm"
                onValueChange={(value) => setPriority(Math.max(0, Math.floor(value ?? 0)))}
              >
                <NumberFieldGroup>
                  <NumberFieldDecrement />
                  <NumberFieldInput aria-label={t('批量设置优先级', 'Set priority for selected accounts')} />
                  <NumberFieldIncrement />
                </NumberFieldGroup>
              </NumberField>
              <Button size="sm" loading={applyPriority.isPending} disabled={busy} onClick={() => applyPriority.mutate(priority)}>
                {t('应用', 'Apply')}
              </Button>
            </div>

            <div className="flex flex-wrap items-center gap-2 lg:border-l lg:pl-3">
              <div className="mr-auto min-w-28">
                <div className="text-xs font-medium">{t('RPM 上限', 'RPM limit')}</div>
                <div className="text-xs text-muted-foreground">{t('每分钟最多转发多少条', 'Max requests forwarded per minute')}</div>
              </div>
              <Select items={rpmModeItems} value={rpmMode} onValueChange={(value) => value && setRpmMode(value as typeof rpmMode)}>
                <SelectTrigger aria-label={t('批量设置 RPM 上限策略', 'Set RPM limit policy for selected accounts')} size="sm" className="min-w-28"><SelectValue /></SelectTrigger>
                <SelectPopup>
                  {rpmModeItems.map((item) => (
                    <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              {rpmMode === 'custom' && (
                <NumberField value={customRpm} min={1} step={1} size="sm" onValueChange={(value) => setCustomRpm(Math.max(1, Math.floor(value ?? 1)))}>
                  <NumberFieldGroup>
                    <NumberFieldDecrement />
                    <NumberFieldInput aria-label={t('批量设置独立 RPM 上限', 'Set a custom RPM limit for selected accounts')} />
                    <NumberFieldIncrement />
                  </NumberFieldGroup>
                </NumberField>
              )}
              <Button size="sm" loading={applyRpmLimit.isPending} disabled={busy} onClick={() => applyRpmLimit.mutate(rpmLimit)}>
                {t('应用', 'Apply')}
              </Button>
            </div>
          </div>
        )}

        <AlertDialog open={confirmDelete} onOpenChange={setConfirmDelete}>
          <AlertDialogPopup>
            <AlertDialogHeader>
              <AlertDialogTitle>
                {t(`删除 ${formattedCount} 个账号`, `Delete ${englishAccountCount}`)}
              </AlertDialogTitle>
              <AlertDialogDescription>
                {t(
                  `确定删除选中的 ${formattedCount} 个账号？历史用量记录与设备绑定将一并清除，且无法恢复。`,
                  `Delete the selected ${englishAccountCount}? Usage history and device bindings will also be removed and cannot be recovered.`,
                )}
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</AlertDialogClose>
              <Button variant="destructive" loading={applyDelete.isPending} onClick={() => applyDelete.mutate()}>
                {t(`删除 ${formattedCount} 个`, `Delete ${formattedCount}`)}
              </Button>
            </AlertDialogFooter>
          </AlertDialogPopup>
        </AlertDialog>
    </Card>
  )
}
