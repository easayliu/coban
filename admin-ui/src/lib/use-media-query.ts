import { useEffect, useState } from 'react'

/**
 * 响应式媒体查询。用在「两套布局只该渲染一套」的地方——纯 CSS 的 `lg:hidden`
 * 会把两套 DOM 都建出来，一份上百行的表格再配一份卡片列表就是白花的节点。
 * 只切换视觉时仍应优先用 CSS 断点，这个 hook 会引入一次额外渲染。
 */
export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(
    () => typeof window !== 'undefined' && window.matchMedia(query).matches,
  )

  useEffect(() => {
    const media = window.matchMedia(query)
    const sync = () => setMatches(media.matches)
    sync()
    media.addEventListener('change', sync)
    return () => media.removeEventListener('change', sync)
  }, [query])

  return matches
}
