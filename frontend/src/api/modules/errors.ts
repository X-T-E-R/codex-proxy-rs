import type { RequestOptions } from '../request'
import request from '../request'

export type ServerErrorPeriod = 'today' | '7d' | '30d'

// 主项按请求去重；503 来源与错误族子项可重叠，不能相加
export interface ServerErrorTotals {
  retainedRequests: number
  serverRelatedRequests: number
  terminalServerRelatedRequests: number
  overloaded: number
  status503: number
  otherServerPrefix: number
  other5xx: number
  upstream503: number
  infrastructure503: number
  noProvider503: number
  unclassified503: number
  client200Errors: number
  legacyMessageMatches: number
}

export interface ServerErrorDaily extends ServerErrorTotals {
  day: string
}

export interface ServerErrorAttempts {
  observed: number
  serverRelated: number
  overloaded: number
  status503: number
  /** 仅覆盖已观测到的尝试，不能当作全部尝试数 */
  coverage: 'observed_only'
}

export interface ServerErrorSummary {
  fromInclusive: string
  toExclusive: string
  timezone: string
  provider: string | null
  timeBasis: string
  source: string
  earliestRetainedAt: string | null
  latestRetainedAt: string | null
  totals: ServerErrorTotals
  daily: ServerErrorDaily[]
  attempts: ServerErrorAttempts
}

interface ServerErrorSummaryParams {
  provider?: string
  period?: ServerErrorPeriod
  asOf?: number
  startTime?: string
  endTime?: string
}

export function getServerErrorSummary(data: ServerErrorSummaryParams, options: RequestOptions = {}) {
  return request<ServerErrorSummary>({
    url: '/api/admin/errors/summary',
    method: 'GET',
    params: data,
    ...options,
  })
}
