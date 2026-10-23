import type { RequestOptions } from '../request'
import type { FastMode } from './groups'
import request from '../request'

// 账号只接受 inherit/enabled/disabled；default 仅作为全局与有效档位的结果出现
export type AccountPriorityMode = 'inherit' | 'enabled' | 'disabled'

export type PriorityStatusReason = 'off' | 'awaiting_baseline' | 'armed' | 'quota_reset'

export interface GlobalPriorityPolicy {
  revision: number
  mode: FastMode
  autoOffOnQuotaReset: boolean
}

export interface AccountPriorityStatus {
  armed: boolean
  lastObservedAt: string | null
  lastAutoOffAt: string | null
  reason: PriorityStatusReason
}

export interface AccountPriorityPolicy {
  accountId: string
  revision: number
  mode: AccountPriorityMode
  /** null 表示继承全局 */
  autoOffOnQuotaReset: boolean | null
  effectiveAutoOffOnQuotaReset: boolean
  effectiveMode: FastMode
  global: GlobalPriorityPolicy
  status: AccountPriorityStatus
}

interface GlobalPriorityPolicyUpdate {
  expectedRevision: number
  mode: FastMode
  autoOffOnQuotaReset: boolean
}

interface AccountPriorityPolicyParams {
  accountId: string
}

interface AccountPriorityPolicyUpdate extends AccountPriorityPolicyParams {
  expectedRevision: number
  mode: AccountPriorityMode
  autoOffOnQuotaReset: boolean | null
}

export function getGlobalPriorityPolicy(options: RequestOptions = {}) {
  return request<GlobalPriorityPolicy>({
    url: '/api/admin/priority-policy',
    method: 'GET',
    ...options,
  })
}

export function updateGlobalPriorityPolicy(data: GlobalPriorityPolicyUpdate, options: RequestOptions = {}) {
  return request<GlobalPriorityPolicy>({
    url: '/api/admin/priority-policy/update',
    method: 'POST',
    data,
    ...options,
  })
}

export function getAccountPriorityPolicy(data: AccountPriorityPolicyParams, options: RequestOptions = {}) {
  return request<AccountPriorityPolicy>({
    url: '/api/admin/accounts/priority-policy',
    method: 'GET',
    params: data,
    ...options,
  })
}

export function updateAccountPriorityPolicy(data: AccountPriorityPolicyUpdate, options: RequestOptions = {}) {
  return request<AccountPriorityPolicy>({
    url: '/api/admin/accounts/priority-policy/update',
    method: 'POST',
    data,
    ...options,
  })
}
