import type { Ref } from 'vue'
import type { UsageTimeRangeParams } from './useUsageTimeRange'
import type { UsageErrorType, UsageRecordStatus } from '@/api'
import type { AnalyticsMetadata } from '@/api/modules/analytics'

import { computed, onMounted, onScopeDispose, shallowRef, watch } from 'vue'
import {
  getUsageRecordInsightsDiagnostics,
  getUsageRecordInsightsOverview,
  getUsageRecords,
  getUsageRecordSummary,
} from '@/api'
import { useStablePagedQuery } from '@/composables/useStablePagedQuery'
import { withMinimumDuration } from '@/utils/operation'
import { useRunningPoll } from './useRunningPoll'

interface UseUsageRecordsTableOptions {
  timeRangeParams: Readonly<Ref<UsageTimeRangeParams>>
  latestTimeRangeParams: (asOf?: number) => UsageTimeRangeParams
  status: Readonly<Ref<UsageRecordStatus>>
}

type UsageLoadScope = 'all' | 'table'
export type UsageRecordFilterField = 'key' | 'account' | 'model' | 'error' | 'requestId' | 'upstreamRequestId' | 'responseId'
// 精确 ID 定位不携带 status，后端据此同时查成功与失败
export function isUsageLocatorField(field: UsageRecordFilterField): boolean {
  return field === 'requestId' || field === 'upstreamRequestId' || field === 'responseId'
}
type LoadingAnalytics<T> = Omit<T, 'analytics'> & { analytics: AnalyticsMetadata | null }

interface UsageLoadOptions {
  scope?: UsageLoadScope
}

export function useUsageRecordsTable(options: UseUsageRecordsTableOptions) {
  const analyticsLoading = shallowRef(true)
  const summary = shallowRef(emptySummary())
  const insights = shallowRef(emptyInsights())
  const filterField = shallowRef<UsageRecordFilterField>('key')
  const filterValue = shallowRef('')
  const errorTypes = shallowRef<UsageErrorType[]>([])
  const filterParams = computed(() => {
    const value = filterValue.value.trim() || undefined
    const selectedErrorTypes = errorTypes.value.length > 0 ? errorTypes.value.join(',') : undefined
    return {
      requestId: filterField.value === 'requestId' ? value : undefined,
      upstreamRequestId: filterField.value === 'upstreamRequestId' ? value : undefined,
      responseId: filterField.value === 'responseId' ? value : undefined,
      clientApiKeyName: filterField.value === 'key' ? value : undefined,
      accountName: filterField.value === 'account' ? value : undefined,
      model: filterField.value === 'model' ? value : undefined,
      error: filterField.value === 'error' ? value : undefined,
      errorTypes: selectedErrorTypes,
    }
  })
  const providerQuery = shallowRef('')
  let tableParams = snapshot()
  const table = useStablePagedQuery({
    initialPageSize: 10,
    load: (pagination, requestOptions) => getUsageRecords({ ...pagination, ...tableParams }, requestOptions),
  })
  const { currentPage, pageSize, loading, error, items: records, total: totalRecords } = table
  const refreshingList = shallowRef(false)
  const diagnosticDimension = shallowRef('model')
  let analyticsRequestId = 0
  let diagnosticRequestId = 0
  let analyticsController: AbortController | undefined
  let diagnosticController: AbortController | undefined
  const scopedParams = () => ({
    ...options.timeRangeParams.value,
    ...(providerQuery.value ? { provider: providerQuery.value } : {}),
  })
  const usagePagination = computed(() => ({
    currentPage: currentPage.value,
    pageSize: pageSize.value,
    total: totalRecords.value,
  }))

  function snapshot(asOf?: number) {
    return {
      ...options.latestTimeRangeParams(asOf),
      provider: providerQuery.value || undefined,
      ...filterParams.value,
      // 精确 ID 定位时省略 status，让后端同时查成功与失败
      status: isUsageLocatorField(filterField.value) ? undefined : options.status.value,
    }
  }

  function filterChanged() {
    const filter = filterParams.value
    return tableParams.requestId !== filter.requestId
      || tableParams.upstreamRequestId !== filter.upstreamRequestId
      || tableParams.responseId !== filter.responseId
      || tableParams.clientApiKeyName !== filter.clientApiKeyName
      || tableParams.accountName !== filter.accountName
      || tableParams.model !== filter.model
      || tableParams.error !== filter.error
      || tableParams.errorTypes !== filter.errorTypes
  }

  function resetTable(asOf?: number) {
    tableParams = snapshot(asOf)
    // 筛选范围变化后不能继续展示上一范围的数据，即使新查询失败。
    records.value = []
    currentPage.value = 1
    totalRecords.value = 0
  }

  async function loadUsageRecords(loadOptions: UsageLoadOptions = {}) {
    const { scope = 'all' } = loadOptions
    const globalParams = scopedParams()
    if (scope === 'all')
      resetTable()

    await Promise.all([
      table.execute(),
      ...(scope === 'all' ? [loadUsageAnalytics(globalParams)] : []),
    ])
  }

  async function loadUsageAnalytics(globalParams: ReturnType<typeof scopedParams>) {
    const requestId = ++analyticsRequestId
    const diagnosticsId = ++diagnosticRequestId
    analyticsController?.abort()
    diagnosticController?.abort()
    analyticsController = new AbortController()
    const requestOptions = { signal: analyticsController.signal }
    const dimension = diagnosticDimension.value
    analyticsLoading.value = true
    summary.value = emptySummary()
    insights.value = emptyInsights()
    try {
      const [nextSummary, overview, diagnostics] = await Promise.all([
        getUsageRecordSummary(globalParams, requestOptions),
        getUsageRecordInsightsOverview(globalParams, requestOptions),
        getUsageRecordInsightsDiagnostics({
          ...globalParams,
          dimension,
        }, requestOptions),
      ])
      if (requestId !== analyticsRequestId)
        return

      summary.value = nextSummary
      insights.value = {
        overview,
        diagnostics:
          diagnosticsId === diagnosticRequestId && dimension === diagnosticDimension.value
            ? diagnostics
            : insights.value.diagnostics,
      }
    }
    catch {
      if (requestId === analyticsRequestId) {
        summary.value = emptySummary()
        insights.value = emptyInsights()
      }
    }
    finally {
      if (requestId === analyticsRequestId) {
        analyticsLoading.value = false
      }
    }
  }

  async function loadDiagnostics() {
    const requestId = ++diagnosticRequestId
    diagnosticController?.abort()
    diagnosticController = new AbortController()
    const dimension = diagnosticDimension.value
    const params = scopedParams()
    try {
      const diagnostics = await getUsageRecordInsightsDiagnostics({
        ...params,
        dimension,
      }, { signal: diagnosticController.signal })
      if (requestId !== diagnosticRequestId || dimension !== diagnosticDimension.value)
        return
      insights.value = {
        ...insights.value,
        diagnostics,
      }
    }
    catch {}
  }

  async function refreshUsageRecords(asOf?: number) {
    if (refreshingList.value || loading.value)
      return
    refreshingList.value = true
    try {
      await withMinimumDuration(() => reloadLatestTable(asOf))
    }
    finally {
      refreshingList.value = false
    }
  }

  function reloadLatestTable(asOf?: number) {
    resetTable(asOf)
    return loadUsageRecords({ scope: 'table' })
  }

  // 当前页只要还有进行中的记录就按固定间隔重取列表，全部转终态即停
  const hasRunningRecords = computed(() => records.value.some(record => record.logicalOutcome === 'running'))
  const runningPoll = useRunningPoll({
    shouldRun: () => hasRunningRecords.value,
    refresh: async () => {
      // 用户主动刷新或翻页在飞时让位，避免轮询取消用户的请求或写回旧页数据
      if (loading.value || refreshingList.value)
        return
      // 沿用当前窗口与筛选，只把屏上记录更新到最新状态
      await table.execute(currentPage.value, { silent: true })
    },
  })

  watch(hasRunningRecords, (running) => {
    if (running)
      runningPoll.start()
    else
      runningPoll.stop()
  })

  function handlePageChange(nextPage: number) {
    if (filterChanged()) {
      void reloadLatestTable()
      return
    }
    void table.execute(nextPage)
  }

  function handlePageSizeChange(nextPageSize: number) {
    pageSize.value = nextPageSize
    if (filterChanged()) {
      void reloadLatestTable()
      return
    }
    void table.reloadFromStart()
  }

  onMounted(() => {
    loadUsageRecords()
  })

  watch(diagnosticDimension, () => {
    void loadDiagnostics()
  })

  watch(providerQuery, () => {
    void loadUsageRecords()
  })

  watch(options.status, () => {
    void reloadLatestTable()
  })

  watch(filterField, () => {
    filterValue.value = ''
  }, { flush: 'sync' })

  watch(filterParams, (_value, _previous, onCleanup) => {
    const timer = setTimeout(() => {
      if (filterChanged())
        void reloadLatestTable()
    }, 250)
    onCleanup(() => clearTimeout(timer))
  })

  onScopeDispose(() => {
    analyticsRequestId += 1
    diagnosticRequestId += 1
    analyticsController?.abort()
    diagnosticController?.abort()
  })

  return {
    currentPage,
    pageSize,
    filterField,
    filterValue,
    errorTypes,
    providerQuery,
    usagePagination,
    loading,
    error,
    analyticsLoading,
    records,
    summary,
    insights,
    refreshingList,
    diagnosticDimension,
    loadUsageRecords,
    refreshUsageRecords,
    handlePageChange,
    handlePageSizeChange,
  }
}

function emptySummary() {
  const summary: LoadingAnalytics<Awaited<ReturnType<typeof getUsageRecordSummary>>> = {
    analytics: null,
    totalRequests: '0',
    inputTokens: '0',
    outputTokens: '0',
    cachedInputTokens: '0',
    cacheWriteInputTokens: '0',
    totalTokens: '0',
    averageLatencyMs: '0 ms',
  }
  return summary
}

function emptyInsights() {
  return {
    overview: emptyOverview(),
    diagnostics: emptyDiagnostics(),
  }
}

function emptyOverview() {
  const overview: LoadingAnalytics<Awaited<ReturnType<typeof getUsageRecordInsightsOverview>>> = {
    analytics: null,
    granularity: '1d',
    health: {
      totalRequests: 0,
      successRequests: 0,
      failedRequests: 0,
      cancelledRequests: 0,
      incompleteRequests: 0,
      callerErrorRequests: 0,
      successRate: 0,
      completionRate: 0,
      requestChangeRate: null,
      successRateChange: null,
      points: [],
    },
    performance: {
      latencyP50Ms: null,
      latencyP95Ms: null,
      latencyP99Ms: null,
      firstTokenP50Ms: null,
      firstTokenP95Ms: null,
      firstTokenP99Ms: null,
      admissionDecisionP50Ms: null,
      admissionDecisionP95Ms: null,
      accountSelectionWaitP50Ms: null,
      accountSelectionWaitP95Ms: null,
      outputThroughputP10: null,
      outputThroughputP50: null,
      outputThroughputP90: null,
      capacityUtilization: null,
      capacityUtilizationP95: null,
      latencyCoverage: 0,
      firstTokenCoverage: 0,
      admissionDecisionCoverage: 0,
      accountSelectionWaitCoverage: 0,
      capacityCoverage: 0,
      points: [],
    },
    cost: {
      estimatedCost: null,
      standardCost: null,
      noCacheCost: null,
      cacheSavings: null,
      tierPremium: null,
      costPerRequest: null,
      costPerSuccessfulRequest: null,
      tokensPerRequest: 0,
      cachedTokenRate: 0,
      cacheHitRequestRate: 0,
      inputTokens: 0,
      outputTokens: 0,
      cachedInputTokens: 0,
      totalTokens: 0,
      points: [],
      coverage: { known: 0, partial: 0, unknown: 0, notBillable: 0 },
    },
  }
  return overview
}

function emptyDiagnostics() {
  const diagnostics: LoadingAnalytics<Awaited<ReturnType<typeof getUsageRecordInsightsDiagnostics>>> = {
    analytics: null,
    dimension: 'model',
    items: [],
  }
  return diagnostics
}
