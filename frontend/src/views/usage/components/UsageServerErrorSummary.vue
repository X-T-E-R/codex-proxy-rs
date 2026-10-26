<script setup lang="ts">
import type { BarSeriesOption, EChartsOption, LineSeriesOption } from 'echarts'
import type { UsageTimeRangeParams } from '../composables/useUsageTimeRange'
import type { ServerErrorDaily } from '@/api'
import { ZButton, ZCard, ZEmpty, ZIconButton, ZPopover, ZSkeleton } from '@codex-proxy/ui'

import { CircleAlert, TriangleAlert } from '@lucide/vue'
import { computed, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import BaseChart from '@/components/charts/BaseChart.vue'
import { tooltipIndex, tooltipRows } from '@/components/charts/tooltip'
import { useChartPalette } from '@/composables/useChartPalette'
import { formatLocalizedCompactNumber as formatCompactNumber, formatInteger } from '@/utils/format'
import { useServerErrorSummary } from '../composables/useServerErrorSummary'
import {
  usageCategoryAxis,
  usageTooltip,
  usageTooltipContent,
  usageTooltipItem,
  usageValueAxis,
} from '../utils/chart'

const props = defineProps<{
  range: UsageTimeRangeParams
  provider?: string
}>()

const emit = defineEmits<{ overloaded: [] }>()

const { t } = useI18n({ useScope: 'global' })

const { summary, loading, error, load } = useServerErrorSummary({ range: () => props.range, provider: () => props.provider })
const { palette } = useChartPalette()

watch([() => props.range, () => props.provider], () => void load(true), { immediate: true })

const totals = computed(() => summary.value?.totals)
const daily = computed<ServerErrorDaily[]>(() => summary.value?.daily ?? [])
const status503 = computed(() => Math.max(0, totals.value?.status503 ?? 0))
const overloaded = computed(() => Math.max(0, totals.value?.overloaded ?? 0))
const hasErrors = computed(() => (totals.value?.serverRelatedRequests ?? 0) > 0
  || (totals.value?.client200Errors ?? 0) > 0)
const hasDaily = computed(() => daily.value.length > 0 && (status503.value > 0 || overloaded.value > 0))

// 503 四来源按请求各自判断，同一请求可同时命中多个来源，因此不能堆叠相加
const sources = computed(() => [
  { key: 'upstream', label: t('usage.serverErrors.sources.upstream'), count: totals.value?.upstream503 ?? 0, color: palette.value.warning },
  { key: 'infrastructure', label: t('usage.serverErrors.sources.infrastructure'), count: totals.value?.infrastructure503 ?? 0, color: palette.value.danger },
  { key: 'noProvider', label: t('usage.serverErrors.sources.noProvider'), count: totals.value?.noProvider503 ?? 0, color: palette.value.info },
  { key: 'unclassified', label: t('usage.serverErrors.sources.unclassified'), count: totals.value?.unclassified503 ?? 0, color: palette.value.normal },
])

const kpis = computed(() => [
  { key: 'overloaded', label: t('usage.serverErrors.kpis.overloaded'), value: overloaded.value, hint: t('usage.serverErrors.kpis.overloadedHint'), accent: true },
  { key: 'status503', label: t('usage.serverErrors.kpis.status503'), value: status503.value, hint: t('usage.serverErrors.kpis.status503Hint'), accent: false },
  { key: 'serverRelated', label: t('usage.serverErrors.kpis.serverRelated'), value: totals.value?.serverRelatedRequests ?? 0, hint: t('usage.serverErrors.kpis.serverRelatedHint'), accent: false },
  { key: 'terminal', label: t('usage.serverErrors.kpis.terminal'), value: totals.value?.terminalServerRelatedRequests ?? 0, hint: t('usage.serverErrors.kpis.terminalHint'), accent: false },
  { key: 'client200', label: t('usage.serverErrors.kpis.client200'), value: totals.value?.client200Errors ?? 0, hint: t('usage.serverErrors.kpis.client200Hint'), accent: false },
])

const secondary = computed(() => [
  { key: 'otherServerPrefix', label: t('usage.serverErrors.families.otherServerPrefix'), value: totals.value?.otherServerPrefix ?? 0, hint: t('usage.serverErrors.families.otherServerPrefixHint') },
  { key: 'other5xx', label: t('usage.serverErrors.families.other5xx'), value: totals.value?.other5xx ?? 0, hint: '' },
  { key: 'legacyMessageMatches', label: t('usage.serverErrors.families.legacyMessageMatches'), value: totals.value?.legacyMessageMatches ?? 0, hint: t('usage.serverErrors.families.legacyMessageMatchesHint') },
])

const dayLabels = computed(() => daily.value.map(item => item.day.slice(5)))

function sourceCount(row: ServerErrorDaily, key: string) {
  switch (key) {
    case 'upstream': return row.upstream503
    case 'infrastructure': return row.infrastructure503
    case 'noProvider': return row.noProvider503
    default: return row.unclassified503
  }
}

const chartOption = computed<EChartsOption>(() => {
  const theme = palette.value
  const rows = daily.value
  const series: Array<BarSeriesOption | LineSeriesOption> = [
    // 来源之间可重叠，并列展示原值，堆叠会虚造 503 总量
    ...sources.value.map(source => ({
      name: source.label,
      type: 'bar' as const,
      data: rows.map(row => sourceCount(row, source.key)),
      barMaxWidth: 10,
      barGap: '15%',
      itemStyle: { color: source.color, opacity: 0.75 },
    })),
    {
      name: t('usage.serverErrors.kpis.overloaded'),
      type: 'line' as const,
      data: rows.map(row => row.overloaded),
      showSymbol: rows.length <= 12,
      symbol: 'circle',
      symbolSize: 5,
      lineStyle: { color: theme.reasoning, width: 2, type: 'dashed' as const },
      itemStyle: { color: theme.reasoning },
      z: 3,
    },
  ]

  return {
    animationDuration: 240,
    grid: { left: 0, right: 0, top: 12, bottom: 0, outerBoundsMode: 'same', outerBoundsContain: 'axisLabel' },
    tooltip: usageTooltip(theme, formatTooltip),
    xAxis: usageCategoryAxis(dayLabels.value, theme),
    yAxis: usageValueAxis(theme, value => formatCompactNumber(value)),
    series,
  }
})

function formatTooltip(params: unknown) {
  const rows = tooltipRows(params)
  const row = daily.value[tooltipIndex(rows[0])]
  if (!row)
    return ''

  const theme = palette.value
  const lines = [
    usageTooltipItem(t('usage.serverErrors.kpis.serverRelated'), formatInteger(row.serverRelatedRequests), theme.textSecondary),
    usageTooltipItem(t('usage.serverErrors.kpis.overloaded'), formatInteger(row.overloaded), theme.reasoning),
    usageTooltipItem(t('usage.serverErrors.kpis.status503'), formatInteger(row.status503), theme.danger),
    ...sources.value
      .filter(source => sourceCount(row, source.key) > 0)
      .map(source => usageTooltipItem(source.label, formatInteger(sourceCount(row, source.key)), source.color)),
    usageTooltipItem(t('usage.serverErrors.kpis.client200'), formatInteger(row.client200Errors), theme.warning),
  ]
  return usageTooltipContent(theme, row.day, lines)
}
</script>

<template>
  <ZCard as="article" class="mt-5" :title="t('usage.serverErrors.title')">
    <template #description>
      <span class="inline-flex items-center gap-1">
        {{ t('usage.serverErrors.description') }}
        <ZPopover class="-my-1" trigger="hover-click" placement="top-start">
          <template #reference="{ open }">
            <ZIconButton
              :aria-label="t('usage.serverErrors.help.label')"
              :title="undefined"
              :aria-expanded="open"
              class="size-6! hover:bg-transparent! active:bg-transparent!"
            >
              <CircleAlert class="size-3.5" aria-hidden="true" />
            </ZIconButton>
          </template>
          <div class="grid max-w-80 gap-2 px-3 py-2 text-cp-sm leading-relaxed text-cp-text-secondary">
            <p>{{ t('usage.serverErrors.help.dedupe') }}</p>
            <p>{{ t('usage.serverErrors.drilldownHint') }}</p>
            <p>{{ t('usage.serverErrors.help.overlap') }}</p>
            <p>{{ t('usage.serverErrors.help.sources') }}</p>
            <p>{{ t('usage.serverErrors.help.client200') }}</p>
            <p>{{ t('usage.serverErrors.help.retention') }}</p>
            <p>{{ t('usage.serverErrors.help.attempts') }}</p>
          </div>
        </ZPopover>
      </span>
    </template>

    <div v-if="loading && !summary" class="grid gap-4" role="status" aria-busy="true">
      <span class="sr-only">{{ t('usage.serverErrors.loading') }}</span>
      <div class="grid grid-cols-2 gap-3 rounded-cp-lg bg-cp-fill-quaternary/45 p-3 sm:grid-cols-5">
        <div v-for="item in 5" :key="item" class="grid gap-2 px-1">
          <ZSkeleton shape="text" class="w-16" />
          <ZSkeleton shape="text" class="w-10" />
        </div>
      </div>
      <ZSkeleton class="h-40 w-full rounded-cp" />
    </div>

    <ZEmpty
      v-else-if="error"
      :title="t('usage.serverErrors.loadFailed')"
      :description="error"
      :icon="TriangleAlert"
      surface="none"
      role="alert"
    >
      <template #action>
        <ZButton :loading="loading" @click="load()">
          {{ t('common.actions.retry') }}
        </ZButton>
      </template>
    </ZEmpty>

    <template v-else-if="summary">
      <ZEmpty
        v-if="!totals || totals.retainedRequests === 0"
        :title="t('usage.serverErrors.emptyNoRecords')"
        :description="t('usage.serverErrors.emptyNoRecordsHint')"
        size="small"
        surface="none"
      />

      <ZEmpty
        v-else-if="!hasErrors"
        :title="t('usage.serverErrors.emptyNoErrors')"
        :description="t('usage.serverErrors.emptyNoErrorsHint', { retained: formatInteger(totals.retainedRequests) })"
        size="small"
        surface="none"
      />

      <div v-else class="grid gap-5">
        <div class="grid grid-cols-2 gap-x-4 gap-y-4 rounded-cp-lg bg-cp-fill-quaternary/45 p-3 sm:grid-cols-5">
          <div v-for="item in kpis" :key="item.key" class="grid min-w-0 content-start gap-1 px-1">
            <span class="truncate text-[10px] font-bold text-cp-text-quaternary">{{ item.label }}</span>
            <button
              v-if="item.key === 'overloaded'"
              type="button"
              class="w-fit cursor-pointer rounded-cp-xs font-mono text-cp-xl leading-none font-heavy text-cp-error-text underline-offset-4 hover:underline focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-cp-primary"
              :disabled="loading"
              :aria-label="t('usage.serverErrors.viewOverloaded', { count: formatInteger(item.value) })"
              @click="emit('overloaded')"
            >
              {{ formatInteger(item.value) }}
            </button>
            <strong
              v-else
              class="font-mono leading-none font-heavy tabular-nums"
              :class="item.accent ? 'text-cp-xl text-cp-error-text' : 'text-cp-lg text-cp-text'"
            >
              {{ formatInteger(item.value) }}
            </strong>
            <span class="text-[10px] leading-3.5 text-cp-text-tertiary">{{ item.hint }}</span>
          </div>
        </div>

        <section v-if="hasDaily" class="grid gap-3">
          <div class="flex flex-wrap items-baseline justify-between gap-2">
            <h3 class="m-0 text-cp-sm font-heavy text-cp-text">
              {{ t('usage.serverErrors.daily.title') }}
            </h3>
            <span class="text-[10px] font-emphasis text-cp-text-quaternary">{{ t('usage.serverErrors.daily.hint') }}</span>
          </div>

          <dl class="flex flex-wrap items-center gap-x-4 gap-y-2 text-cp-xs" :aria-label="t('usage.serverErrors.daily.legend')">
            <div v-for="source in sources" :key="source.key" class="flex items-center gap-1.5">
              <i class="size-1.75 shrink-0 rounded-full" :style="{ backgroundColor: source.color }" />
              <dt class="font-emphasis text-cp-text-secondary">
                {{ source.label }}
              </dt>
              <dd class="m-0 font-mono font-heavy tabular-nums text-cp-text">
                {{ formatInteger(source.count) }}
              </dd>
            </div>
            <div class="flex items-center gap-1.5">
              <i class="h-0.5 w-3 shrink-0 rounded-full" :style="{ backgroundColor: palette.reasoning }" />
              <dt class="font-emphasis text-cp-text-secondary">
                {{ t('usage.serverErrors.kpis.overloaded') }}
              </dt>
              <dd class="m-0 font-mono font-heavy tabular-nums text-cp-text">
                {{ formatInteger(overloaded) }}
              </dd>
            </div>
          </dl>

          <div role="img" :aria-label="t('usage.serverErrors.daily.aria', { overloaded: formatInteger(overloaded), status503: formatInteger(status503) })">
            <BaseChart :option="chartOption" :height="200" />
          </div>
        </section>

        <p v-if="secondary.some(item => item.value > 0)" class="m-0 flex flex-wrap items-center gap-x-4 gap-y-1 text-cp-xs text-cp-text-secondary">
          <span v-for="item in secondary.filter(entry => entry.value > 0)" :key="item.key" class="flex items-center gap-1.5">
            <span class="font-emphasis">{{ item.label }}</span>
            <strong class="font-mono tabular-nums text-cp-text">{{ formatInteger(item.value) }}</strong>
            <span v-if="item.hint" class="text-cp-text-quaternary">{{ item.hint }}</span>
          </span>
        </p>

        <div class="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 text-[10px] leading-4 text-cp-text-tertiary">
          <span>{{ t('usage.serverErrors.footer.retention', { timezone: summary.timezone }) }}</span>
          <span class="font-mono tabular-nums">
            {{ t('usage.serverErrors.footer.attempts', {
              observed: formatInteger(summary.attempts.observed),
              serverRelated: formatInteger(summary.attempts.serverRelated),
              overloaded: formatInteger(summary.attempts.overloaded),
              status503: formatInteger(summary.attempts.status503),
            }) }}
          </span>
        </div>
      </div>
    </template>
  </ZCard>
</template>
