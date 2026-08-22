import { useCallback, useSyncExternalStore } from 'react'

/**
 * 订阅一条媒体查询。**只在布局与「渲染什么」有关时才用它**——纯样式差异交给 CSS，
 * JS 里再判一遍就是同一个断点写在两处，迟早对不上。
 *
 * 这里要的是后者：列表视图在窄屏下不是「长得不同」，而是**不该渲染**（十来列的表压到手机上
 * 每列只剩二十几个像素），所以得由 JS 决定渲染卡片还是表格。
 *
 * 用 useSyncExternalStore 而不是 useState + useEffect：后者首帧先按默认值渲染一遍再纠正，
 * 手机上会看见表格闪一下才变成卡片。SSR/无 matchMedia 时回退成 false（窄屏那一档更安全）。
 */
export function useMediaQuery(query: string): boolean {
  const subscribe = useCallback((onChange: () => void) => {
    if (typeof window === 'undefined' || !window.matchMedia) return () => {}
    const list = window.matchMedia(query)
    list.addEventListener('change', onChange)
    return () => list.removeEventListener('change', onChange)
  }, [query])

  return useSyncExternalStore(
    subscribe,
    () => (typeof window !== 'undefined' && window.matchMedia
      ? window.matchMedia(query).matches
      : false),
    () => false,
  )
}
