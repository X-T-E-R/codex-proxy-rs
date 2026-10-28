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
const statusExplicit = shallowRef(false)
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

const errorExploration = computed(() => recordView.value === 'error' || filterField.value === 'error'
  || (filterField.value === 'search' && filterValue.value.trim() !== '') || errorTypes.value.length > 0)
const recordColumns = computed<TableColumn<UsageListRecord>[]>(() => errorExploration.value
  ? usageRecordColumns.value.flatMap(column => column.key === 'route'
      ? [
          column,
          { key: 'errorMessage', label: t('usage.filters.error'), kind: 'custom', size: '4xl', hideable: false },
          { key: 'upstreamSendState', label: t('usage.error.sendStatus'), kind: 'custom', size: 'md', hideable: false },
        ]
      : [column])
  : usageRecordColumns.value)
const { visibleColumns, columnOptions, setColumnVisible, setColumnOrder, resetColumns } = useTableColumns(recordColumns, 'usage-records')

function showAllOverloaded() {
  // 摘要只按时间与平台统计，钻取清除不属于摘要范围的局部筛选
  filterField.value = 'error'
  filterValue.value = ''
  errorTypes.value = ['overloaded']
  recordView.value = 'all'
  statusExplicit.value = false
}

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
})

// 新错误条件从全部状态开始，显式状态选择只约束当前错误条件
watch([
  () => [...errorTypes.value].sort().join(','),
  () => {
    const value = filterValue.value.trim()
    return value && (filterField.value === 'error' || filterField.value === 'search')
      ? `${filterField.value}:${value}`
      : ''
  },
  filterField,
], ([types, keyword, field], [previousTypes, previousKeyword]) => {
  if ((types && types !== previousTypes) || (keyword && keyword !== previousKeyword))
    statusExplicit.value = false
  if (!statusExplicit.value && (types || keyword || field === 'error'))
    recordView.value = 'all'
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
      v-if="errorExploration"
      :range="timeRangeParams"
      :provider="providerQuery || undefined"
      @overloaded="showAllOverloaded"
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
              <UsageRecordStatusFilter v-model="recordView" @update:model-value="statusExplicit = true" />
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
