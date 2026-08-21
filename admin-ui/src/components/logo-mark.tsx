import { cn } from '@/lib/utils'

export function LogoMark({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      aria-hidden="true"
      className={cn('relative z-10', className)}
    >
      <path d="M5 7.75 12 3.75l7 4-7 4-7-4Z" fill="currentColor" />
      <path d="M5 10.15 10.9 13.5v6.75L5 16.9v-6.75Z" fill="currentColor" opacity=".72" />
      <path d="m19 10.15-5.9 3.35v6.75L19 16.9v-6.75Z" fill="currentColor" opacity=".92" />
    </svg>
  )
}
