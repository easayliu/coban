import { useEffect, useState } from 'react'

/**
 * 延迟跟随的值：`value` 停止变化 `delay` 毫秒后才更新返回值。
 *
 * 用于把「输入框实时回显」和「据此重算列表」拆开——输入框仍绑原值保持跟手，
 * 筛选/排序则用这个延迟值，连续敲键时不会每个字符都把全量账号过一遍。
 */
export function useDebounced<T>(value: T, delay = 200): T {
  const [debounced, setDebounced] = useState(value)
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), delay)
    return () => clearTimeout(t)
  }, [value, delay])
  return debounced
}
