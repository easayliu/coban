import type { ReactNode } from 'react'
import type { LucideIcon } from 'lucide-react'
import {
  Frame,
  FrameDescription,
  FrameHeader,
  FramePanel,
  FrameTitle,
} from '@/components/ui/frame'

/**
 * 系统设置页的统一分组容器。
 *
 * Frame 是仓库中来自 Coss UI 的设置面板底座；这里仅补齐标题语义、图标和说明，
 * 避免接入、设备与转发页面各自再造一套卡片样式。
 */
export function SettingsGroup({
  icon: Icon,
  title,
  description,
  children,
}: {
  icon?: LucideIcon
  title: string
  description?: string
  children: ReactNode
}) {
  return (
    <Frame>
      <FrameHeader className="gap-1.5">
        <div className="flex items-center gap-2">
          {Icon && <Icon aria-hidden="true" className="size-4 text-muted-foreground" />}
          <FrameTitle>
            <h3>{title}</h3>
          </FrameTitle>
        </div>
        {description && (
          <FrameDescription className={Icon ? 'pl-6 text-xs leading-5' : 'text-xs leading-5'}>
            {description}
          </FrameDescription>
        )}
      </FrameHeader>
      <FramePanel className="divide-y p-0">{children}</FramePanel>
    </Frame>
  )
}
