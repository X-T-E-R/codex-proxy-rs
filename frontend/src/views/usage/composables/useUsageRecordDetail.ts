import type { UsageListRecord, UsageRecordDetail } from '@/api'
import { onScopeDispose, shallowRef, watch } from 'vue'
import { getUsageRecordDetail } from '@/api'

import { errorMessage } from '@/utils/operation'

export function useUsageRecordDetail() {
  const showDetailModal = shallowRef(false)
  const selectedUsageRecord = shallowRef<UsageRecordDetail | null>(null)
  const pendingUsageRecord = shallowRef<UsageListRecord | null>(null)
  const detailLoading = shallowRef(false)
  const detailError = shallowRef('')
  let sequence = 0
  let controller: AbortController | undefined

  // 关闭或换选后旧请求立即失效，迟到结果不再写入当前选中项
  function cancel() {
    sequence += 1
    controller?.abort()
    controller = undefined
    detailLoading.value = false
  }

  async function load(record: UsageListRecord) {
    const current = ++sequence
    controller?.abort()
    const next = new AbortController()
    controller = next
    pendingUsageRecord.value = record
    selectedUsageRecord.value = null
    detailError.value = ''
    detailLoading.value = true
    try {
      const detail = await getUsageRecordDetail({ id: record.id }, { silent: true, signal: next.signal })
      if (current !== sequence)
        return
      selectedUsageRecord.value = detail
    }
    catch (cause: unknown) {
      if (current !== sequence || next.signal.aborted)
        return
      detailError.value = errorMessage(cause)
    }
    finally {
      if (current === sequence) {
        detailLoading.value = false
        controller = undefined
      }
    }
  }

  function handleViewDetail(record: UsageListRecord) {
    // 先打开弹窗，详情请求进行期间由弹窗自己展示 loading 与定位信息
    showDetailModal.value = true
    void load(record)
  }

  function retryDetail() {
    if (pendingUsageRecord.value && !detailLoading.value)
      void load(pendingUsageRecord.value)
  }

  watch(showDetailModal, (open) => {
    if (!open)
      cancel()
  }, { flush: 'sync' })

  onScopeDispose(cancel)

  return {
    showDetailModal,
    selectedUsageRecord,
    pendingUsageRecord,
    detailLoading,
    detailError,
    handleViewDetail,
    retryDetail,
  }
}
