import { useCallback, useState } from 'react'

/**
 * 存到 localStorage 的界面偏好，刷新后保持上次选择。
 *
 * 放入用户主动选择的列表工作上下文（视图、排序、每页条数、状态筛选与搜索词），
 * 让页面刷新后仍保持相同结果集。
 */
const PREFIX = 'coban.'

/** localStorage 在隐私模式/禁用存储时读写会抛异常，一律降级为「不持久化」。 */
function read(key: string): string | null {
  try {
    return localStorage.getItem(PREFIX + key)
  } catch {
    return null
  }
}

function write(key: string, value: string): void {
  try {
    localStorage.setItem(PREFIX + key, value)
  } catch {
    // 存不下就算了，不影响当次使用
  }
}

/**
 * 与 `useState` 同签名，但值会写入 localStorage 并在下次加载时恢复。
 *
 * `parse` 负责把存下来的字符串还原成合法值——存量值可能来自旧版本或被手改过，
 * 校验不通过时返回 `null`，本 hook 会退回 `fallback`，避免脏值让界面进入无效状态。
 */
export function usePersisted<T>(
  key: string,
  fallback: T,
  parse: (raw: string) => T | null,
  serialize: (v: T) => string = String,
  /**
   * 首屏优先值，通常来自 URL。链接里带的检索条件必须压过 localStorage：
   * 别人发来的链接理应打开成他看到的那个结果集，而不是本机上次留下的筛选。
   */
  seed?: string | null,
): [T, (v: T) => void] {
  const [value, setValue] = useState<T>(() => {
    if (seed != null) {
      const seeded = parse(seed)
      if (seeded != null) {
        write(key, serialize(seeded))
        return seeded
      }
    }
    const raw = read(key)
    if (raw == null) return fallback
    return parse(raw) ?? fallback
  })
  const set = useCallback(
    (v: T) => {
      setValue(v)
      write(key, serialize(v))
    },
    // serialize 通常是内联箭头函数，每次渲染都是新引用；此处只按 key 记忆，
    // 保证 setter 引用稳定（serialize 的行为在同一 key 下不会变）。
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [key],
  )
  return [value, set]
}

/** 常用 parse：值必须落在给定候选集内，否则视为脏值。 */
export function oneOf<T extends string>(allowed: readonly T[]) {
  return (raw: string): T | null => (allowed as readonly string[]).includes(raw) ? (raw as T) : null
}

/** 常用 parse：数字且必须落在给定候选集内。 */
export function numberOneOf(allowed: readonly number[]) {
  return (raw: string): number | null => {
    const n = Number(raw)
    return allowed.includes(n) ? n : null
  }
}
