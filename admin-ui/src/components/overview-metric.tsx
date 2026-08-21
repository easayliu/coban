import type { ElementType } from 'react'
import { cn } from '@/lib/utils'
import { Skeleton } from '@/components/ui/skeleton'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

export function OverviewMetric({
  label, value, status, icon: Icon, tone, active = false, onClick, className,
}: {
  label: string
  value: number | string
  status?: string
  icon: ElementType<{ className?: string }>
  tone: 'ok' | 'bad' | 'warn' | 'neutral'
  active?: boolean
  onClick?: () => void
  className?: string
}) {
  const iconClass = {
    ok: 'text-success-foreground',
    bad: 'text-destructive-foreground',
    warn: 'text-warning-foreground',
    neutral: 'text-muted-foreground',
  }[tone]
  const content = (
    <div className="flex min-h-16 items-center gap-3 px-3 py-2.5 sm:px-4">
      <span className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-muted">
        <Icon className={cn('size-4', iconClass)} aria-hidden />
      </span>
      {/* label 在上、数值在下且左对齐：四格宽度不同，数值若各自贴右就散在四个位置，
          横着扫一眼比不出大小。左对齐后四个数字落在同一条起始线上。 */}
      <div className="min-w-0 flex-1">
        <p className="min-w-0 truncate text-xs font-medium text-muted-foreground">{label}</p>
        <div className="mt-1 flex min-w-0 items-baseline gap-2">
          <span className="shrink-0 text-lg font-semibold leading-none tracking-tight tnum">
            {value}
          </span>
          {status && (
            <span className="min-w-0 truncate text-2xs text-muted-foreground" title={status}>
              {status}
            </span>
          )}
        </div>
      </div>
    </div>
  )

  const rootClass = cn(
    'min-w-0 text-left transition-colors',
    onClick && 'cursor-pointer hover:bg-muted/40 focus-visible:relative focus-visible:z-10 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring',
    active && 'bg-muted/72',
    className,
  )

  if (onClick) {
    return (
      <button type="button" className={rootClass} onClick={onClick} aria-pressed={active}>
        {content}
      </button>
    )
  }

  return <div className={rootClass}>{content}</div>
}

/**
 * 概览里的实时格：和 [OverviewMetric] 同一套图标 / 字号，但排布不同。
 *
 * 它是唯一一格「此刻代理在干什么」，手机上又占满一整行，照 label 在上、数值在下排就会在右边空出
 * 半行；这里改成标签贴左、数值贴右的横条，把整行用满。sm 起整行宽到两端拉不住，退回和邻居一样的
 * 竖排。数值旁边的单位与在途数一路跟着走，不再挤进那行会被截断的 status 文案里。
 */
export function LiveTrafficMetric({
  label, value, unit, detail, live, hint, icon: Icon, className,
}: {
  label: string
  value: number | string
  unit: string
  detail: string
  /** 有在途请求：图标转成成功色并点亮呼吸点，静默时保持中性，避免恒亮的绿色变成背景噪声。 */
  live: boolean
  hint: string
  icon: ElementType<{ className?: string }>
  className?: string
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={<div />}
        className={cn(
          'flex min-h-16 min-w-0 cursor-help items-center gap-3 px-3 py-2.5 text-left sm:px-4',
          className,
        )}
      >
        <span className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-muted">
          <Icon className={cn('size-4', live ? 'text-success-foreground' : 'text-muted-foreground')} aria-hidden />
        </span>
        <div className="flex min-w-0 flex-1 flex-wrap items-baseline justify-between gap-x-3 gap-y-1 sm:block">
          <p className="min-w-0 truncate text-xs font-medium text-muted-foreground">{label}</p>
          <div className="flex min-w-0 items-baseline gap-1.5 sm:mt-1">
            <span className="shrink-0 text-lg font-semibold leading-none tracking-tight tnum">
              {value}
            </span>
            <span className="shrink-0 text-2xs text-muted-foreground tracking-wide">{unit}</span>
            <span className="flex min-w-0 items-baseline gap-1.5 text-2xs text-muted-foreground">
              {/* 有在途时呼吸点就是分隔符本身，再补一个中点只是噪声。 */}
              {live ? (
                <span className="relative flex size-1.5 shrink-0 translate-y-[-1px]" aria-hidden>
                  <span className="absolute inline-flex size-full animate-ping rounded-full bg-success opacity-60 motion-reduce:hidden" />
                  <span className="relative inline-flex size-1.5 rounded-full bg-success" />
                </span>
              ) : (
                <span aria-hidden>·</span>
              )}
              <span className="truncate">{detail}</span>
            </span>
          </div>
        </div>
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">{hint}</TooltipPopup>
    </Tooltip>
  )
}

export function OverviewMetricSkeleton({ className }: { className?: string }) {
  return (
    <div className={cn('flex min-h-16 min-w-0 items-center gap-3 px-3 py-2.5 sm:px-4', className)}>
      <Skeleton className="size-8 shrink-0 rounded-lg" />
      <div className="min-w-0 flex-1">
        <Skeleton className="h-3 w-20" />
        <div className="mt-1.5 flex items-center gap-2">
          <Skeleton className="h-5 w-10 shrink-0" />
          <Skeleton className="h-3 w-20 max-w-full" />
        </div>
      </div>
    </div>
  )
}
