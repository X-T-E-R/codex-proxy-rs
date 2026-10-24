<script setup lang="ts">
import type { TableColumn } from '@codex-proxy/ui'

import type { UsageListRecord, UsageRecordStatus } from '@/api'
import { useTableColumns, ZCard, ZDatePicker, ZIconButton, ZPageHeader, ZPagination, ZTableColumnSettings } from '@codex-proxy/ui'
import { Eye } from '@lucide/vue'

import { computed, shallowRef, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import ProviderFilter from '@/components/ProviderFilter.vue'
import { usageRecordColumns } from '@/components/usage/shared/columns'
import UsageRecordsTable from '@/components/usage/UsageRecordsTable.vue'
import UsageRecordStatusFilter from '@/components/usage/UsageRecordStatusFilter.vue'
import UsageErrorDetailModal from './components/UsageErrorDetailModal.vue'
import UsageFilters from './components/UsageFilters.vue'
import UsageInsightsGrid from './components/UsageInsightsGrid.vue'
import UsageRecordDetailModal from './components/UsageRecordDetailModal.vue'
import UsageServerErrorSummary from './components/UsageServerErrorSummary.vue'
import UsageSummaryCards from './components/UsageSummaryCards.vue'
import { useUsageRecordDetail } from './composables/useUsageRecordDetail'
import { isUsageLocatorField, useUsageRecordsTable } from './composables/useUsageRecordsTable'
import { useUsageTimeRange } from './composables/useUsageTimeRange'
import { isErrorUsageRecord } from './utils/detail'

const { t } = useI18n({ useScope: 'global' })

const recordView = shallowRef<UsageRecordStatus>('success')
const recordColumns = computed<TableColumn<UsageListRecord>[]>(() => recordView.value === 'error'
  ? usageRecordColumns.value.flatMap(column => column.key === 'route'
      ? [column, { key: 'errorMessage', label: t('usage.filters.error'), kind: 'custom', size: '4xl', hideable: false }]
      : [column])
  : usageRecordColumns.value)
const { visibleColumns, columnOptions, setColumnVisible, setColumnOrder, resetColumns } = useTableColumns(recordColumns, 'usage-records')
const { timeRange, timeRangeParams, disabledDate, refreshTimeRangeEnd, latestTimeRangeParams }
  = useUsageTimeRange()

const {
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
} = useUsageRecordsTable({
  timeRangeParams,
  latestTimeRangeParams,
  status: recordView,
})

const { showDetailModal, selectedUsageRecord, pendingUsageRecord, detailLoading, detailError, handleViewDetail, retryDetail } = useUsageRecordDetail()
const detailComponent = computed(() => isErrorUsageRecord(pendingUsageRecord.value)
  ? UsageErrorDetailModal
  : UsageRecordDetailModal)

// 刷新先推进窗口末端，列表与摘要共用同一 asOf，避免摘要停留在页面打开时的截止时间
function refreshUsage() {
  const asOf = refreshTimeRangeEnd()
  void refreshUsageRecords(asOf)
}

watch(timeRange, () => {
  refreshTimeRangeEnd()
  void loadUsageRecords()
})

// 精确 ID 定位跨越成功与失败，视图同步回到全部状态避免提示与实际结果不一致
watch(filterField, (field) => {
  if (isUsageLocatorField(field) && recordView.value !== 'all')
    recordView.value = 'all'
  else if (field === 'error' && recordView.value === 'success')
    recordView.value = 'error'
})

// 选择错误类型筛选时自动从仅成功切到错误视图，展示错误列与错误摘要
watch(errorTypes, (types) => {
  if (types.length > 0 && recordView.value === 'success')
    recordView.value = 'error'
})
</script>

<template>
  <div class="w-full">
    <ZPageHeader :title="t('navigation.usage')" :description="t('usage.page.description')">
      <template #actions>
        <ZDatePicker
          v-model="timeRange"
          type="daterange"
          :clearable="false"
          :editable="false"
          :disabled-date="disabledDate"
          single-panel
          :aria-label="t('usage.page.dateRangeLabel')"
          class="w-60 max-w-full"
        />
        <ProviderFilter
          v-model="providerQuery"
          :disabled="refreshingList"
          class="shrink-0"
        />
      </template>
    </ZPageHeader>

    <UsageSummaryCards :summary="summary" />
    <UsageInsightsGrid
      v-model:diagnostic-dimension="diagnosticDimension"
      :overview="insights.overview"
      :diagnostics="insights.diagnostics"
      :loading="analyticsLoading"
    />
    <UsageServerErrorSummary
      v-if="recordView === 'error'"
      :range="timeRangeParams"
    />

    <ZCard
      class="mt-5 flex flex-col"
    >
      <template #header>
        <div>
          <h2 class="m-0 text-xl leading-[1.15] font-heavy text-cp-text">
            {{ t('usage.page.requestDetails') }}
          </h2>
          <p
            class="mt-1.75 mb-0 text-cp leading-[1.15] font-emphasis text-cp-text-secondary"
          >
            {{ t('usage.page.recordsDescription') }}
          </p>
        </div>
      </template>

      <template #body>
        <div
          class="grid min-h-130 min-w-0 flex-1 grid-rows-[auto_minmax(0,1fr)] gap-3"
        >
          <UsageFilters
            v-model:field="filterField"
            v-model:value="filterValue"
            v-model:error-types="errorTypes"
            :loading="loading"
            :refreshing="refreshingList"
            @refresh="refreshUsage"
          >
            <template #actions>
              <UsageRecordStatusFilter v-model="recordView" />
              <ZTableColumnSettings
                :options="columnOptions"
                @change="setColumnVisible"
                @reorder="setColumnOrder"
                @reset="resetColumns"
              />
            </template>
          </UsageFilters>

          <div class="flex min-h-0 min-w-0 flex-col">
            <UsageRecordsTable
              class="min-h-0 flex-1"
              :columns="visibleColumns"
              :rows="records"
              :loading="loading"
              :empty-text="error ? t('usage.page.failedToLoad', { error }) : t('usage.page.noRequestsMatchTheseFilters')"
            >
              <template #actions="{ row }">
                <div class="flex items-center justify-start">
                  <ZIconButton
                    size="small"
                    :aria-label="isErrorUsageRecord(row) ? t('usage.page.viewErrorDetails') : t('usage.page.viewUsageRecordDetails')"
                    @click="handleViewDetail(row)"
                  >
                    <Eye class="size-3.5" />
                  </ZIconButton>
                </div>
              </template>
            </UsageRecordsTable>
            <ZPagination
              v-bind="usagePagination"
              :disabled="loading"
              @current-change="handlePageChange"
              @size-change="handlePageSizeChange"
            />
          </div>
        </div>
      </template>
    </ZCard>

    <component
      :is="detailComponent"
      v-model="showDetailModal"
      :record="selectedUsageRecord"
      :loading="detailLoading"
      :error="detailError"
      :target="pendingUsageRecord"
      @retry="retryDetail"
    />
  </div>
</template>
