import { api } from './client'

export interface AuthState {
  /** 是否已设置管理密码（true = 需登录）。 */
  configured: boolean
  /** 是否由环境变量接管（true = 网页不可改）。 */
  env_managed: boolean
}

/** 鉴权状态（公开接口）。 */
export async function getAuthState(): Promise<AuthState> {
  const { data } = await api.get<AuthState>('/auth/state')
  return data
}

/** 校验登录密码。 */
export async function login(password: string): Promise<void> {
  await api.post('/auth/login', { password })
}

/** 首次设置管理密码。 */
export async function setup(password: string): Promise<void> {
  await api.post('/auth/setup', { password })
}

/** 修改/清除管理密码（空串=清除，已鉴权）。 */
export async function changePassword(password: string): Promise<void> {
  await api.post('/auth/password', { password })
}
