import type { UsageTimeRangeParams } from './useUsageTimeRange'
import type { ServerErrorSummary } from '@/api'
import { onScopeDispose, shallowRef } from 'vue'
import { getServerErrorSummary } from '@/api'

import { errorMessage } from '@/utils/operation'

export function useServerErrorSummary(options: { range: () => UsageTimeRangeParams, provider: () => string | undefined }) {
  const summary = shallowRef<ServerErrorSummary>()
  const loading = shallowRef(true)
  const error = shallowRef('')
  let sequence = 0
  let controller: AbortController | undefined

  // 窗口变化时清空旧窗口数值，刷新同一窗口时保留现有结果直到新结果到达
  async function load(reset = false) {
    const current = ++sequence
    controller?.abort()
    const next = new AbortController()
    controller = next
    const range = options.range()
    loading.value = true
    error.value = ''
    if (reset)
      summary.value = undefined
    try {
      const result = await getServerErrorSummary({
        startTime: range.startTime,
        endTime: range.endTime,
        provider: options.provider(),
      }, { silent: true, signal: next.signal })
      if (current !== sequence)
        return
      summary.value = result
    }
    catch (cause: unknown) {
      if (current !== sequence || next.signal.aborted)
        return
      summary.value = undefined
      error.value = errorMessage(cause)
    }
    finally {
      if (current === sequence)
        loading.value = false
    }
  }

  onScopeDispose(() => {
    sequence += 1
    controller?.abort()
  })

  return { summary, loading, error, load }
}
