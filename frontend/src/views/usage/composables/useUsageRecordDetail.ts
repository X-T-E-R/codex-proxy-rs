import type { UsageDisplayRecord, UsageViewModel } from '../utils/records'
import { shallowRef, watch } from 'vue'
import { getUsageRecordDetail } from '@/api'
import { errorMessage } from '@/utils/async'
import { normalizeUsageRecord } from '../utils/records'

export function useUsageRecordDetail() {
  const showDetailModal = shallowRef(false)
  const selectedUsageRecord = shallowRef<UsageViewModel | null>(null)
  const detailLoading = shallowRef(false)
  const detailError = shallowRef('')
  let selectedId: string | null = null
  let requestId = 0

  async function handleViewDetail(record: UsageDisplayRecord) {
    selectedId = record.id
    selectedUsageRecord.value = null
    detailError.value = ''
    showDetailModal.value = true
    await loadDetail()
  }

  async function loadDetail() {
    if (!selectedId)
      return
    const currentRequest = ++requestId
    detailLoading.value = true
    detailError.value = ''
    try {
      const detail = await getUsageRecordDetail({ id: selectedId })
      if (currentRequest !== requestId)
        return
      selectedUsageRecord.value = normalizeUsageRecord(detail)
    }
    catch (error: unknown) {
      if (currentRequest === requestId)
        detailError.value = errorMessage(error, '加载详情失败')
    }
    finally {
      if (currentRequest === requestId)
        detailLoading.value = false
    }
  }

  watch(showDetailModal, (open) => {
    if (!open) {
      requestId++
      selectedId = null
      selectedUsageRecord.value = null
    }
  })

  return {
    showDetailModal,
    selectedUsageRecord,
    detailLoading,
    detailError,
    handleViewDetail,
    loadDetail,
  }
}
