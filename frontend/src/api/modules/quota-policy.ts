import type { RequestOptions } from '../request'
import request from '../request'

export type QuotaPolicyAction = 'off' | 'stop' | 'reset_then_stop'
export type AccountQuotaPolicyMode = 'inherit' | 'disabled' | 'custom'

export interface QuotaWindowPolicy {
  action: QuotaPolicyAction
  thresholdPercent: number
}

export interface QuotaPolicy {
  primary: QuotaWindowPolicy
  secondary: QuotaWindowPolicy
  autoReset: {
    maxAttemptsPer24h: number
    cooldownSeconds: number
  }
}

export interface GlobalQuotaPolicy {
  revision: number
  policy: QuotaPolicy
}

export interface AccountQuotaPolicy {
  accountId: string
  revision: number
  mode: AccountQuotaPolicyMode
  policy: QuotaPolicy | null
  effectivePolicy: QuotaPolicy
  source: 'global' | 'disabled' | 'account'
  status: {
    paused: boolean
    reason: string | null
    observedAt: string | null
    lastAutoAttemptAt: string | null
    autoAttemptsLast24h: number
    pendingOperationId: string | null
  }
}

export function getGlobalQuotaPolicy(options: RequestOptions = {}) {
  return request<GlobalQuotaPolicy>({
    url: '/api/admin/quota-policy',
    method: 'GET',
    ...options,
  })
}

export function updateGlobalQuotaPolicy(data: { expectedRevision: number, policy: QuotaPolicy }, options: RequestOptions = {}) {
  return request<GlobalQuotaPolicy>({
    url: '/api/admin/quota-policy/update',
    method: 'POST',
    data,
    ...options,
  })
}

export function getAccountQuotaPolicy(params: { accountId: string }, options: RequestOptions = {}) {
  return request<AccountQuotaPolicy>({
    url: '/api/admin/accounts/quota-policy',
    method: 'GET',
    params,
    ...options,
  })
}

export function updateAccountQuotaPolicy(data: {
  accountId: string
  expectedRevision: number
  mode: AccountQuotaPolicyMode
  policy: QuotaPolicy | null
}, options: RequestOptions = {}) {
  return request<AccountQuotaPolicy>({
    url: '/api/admin/accounts/quota-policy/update',
    method: 'POST',
    data,
    ...options,
  })
}
