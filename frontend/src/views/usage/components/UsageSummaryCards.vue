<script setup lang="ts">
import type { getUsageRecordSummary } from '@/api'
import { Activity, Database, FileText, Timer } from '@lucide/vue'

import { computed } from 'vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseMotionIcon from '@/components/base/BaseMotionIcon.vue'

const props = defineProps<{
  summary: Awaited<ReturnType<typeof getUsageRecordSummary>>
  loading?: boolean
  error?: string
}>()

function rateDisplay(value: number | null) {
  return value === null ? '无数据' : `${(value * 100).toFixed(1)}%`
}

function averageLatencyDisplay(value: string) {
  return !value || value === '—' || value === '-' ? '0 ms' : value
}

const items = computed(() => [
  {
    key: 'requests',
    label: '成功请求',
    icon: Activity,
    value: props.summary.totalRequests,
    detail: '筛选范围内',
    tone: 'bg-cp-blue-container text-cp-blue-on-container',
  },
  {
    key: 'tokens',
    label: '总 Token',
    icon: FileText,
    value: props.summary.totalTokens,
    detail: `输入 ${props.summary.inputTokens} / 输出 ${props.summary.outputTokens}`,
    tone: 'bg-cp-green-container text-cp-green-on-container',
  },
  {
    key: 'cached',
    label: '缓存 Token',
    icon: Database,
    value: props.summary.cachedTokens,
    detail: '缓存读取命中',
    tone: 'bg-cp-orange-container text-cp-orange-on-container',
  },
  {
    key: 'latency',
    label: '平均耗时',
    icon: Timer,
    value: averageLatencyDisplay(props.summary.averageLatencyMs),
    detail: '成功请求平均值',
    tone: 'bg-cp-cyan-container text-cp-cyan-on-container',
  },
])

interface TurnStateStat {
  label: string
  value: string
  hint?: string
}

const turnStateGroups = computed<{ title: string, description: string, items: TurnStateStat[] }[]>(() => [
  {
    title: '实际发送给上游',
    description: '从最终 HTTP Header 或 WebSocket 载荷记录，不从账号当前配置倒推。',
    items: [
      { label: '已保存发送值', value: props.summary.turnState.sentCount.toLocaleString('zh-CN') },
      { label: '仅发送、未返回', value: props.summary.turnState.onlySent.toLocaleString('zh-CN') },
      { label: '历史发送证据未知', value: props.summary.turnState.unknown.toLocaleString('zh-CN') },
    ],
  },
  {
    title: '上游实际返回',
    description: '按上游响应中的 X-Codex-Turn-State 统计，与发送值分开计数。',
    items: [
      { label: '已保存返回值', value: props.summary.turnState.returnedCount.toLocaleString('zh-CN') },
      { label: '返回 292 B', value: props.summary.turnState.observed292.toLocaleString('zh-CN') },
      { label: '返回其他长度', value: props.summary.turnState.observedOther.toLocaleString('zh-CN') },
      { label: '无返回值', value: props.summary.turnState.unobserved.toLocaleString('zh-CN') },
      { label: '历史未采集返回值', value: props.summary.turnState.notCollected.toLocaleString('zh-CN') },
      { label: '返回 292 B 比例', value: rateDisplay(props.summary.turnState.hitRate), hint: '返回 292 B ÷（返回 292 B + 返回其他长度）；分母为 0 时显示无数据' },
      { label: '返回采集覆盖率', value: rateDisplay(props.summary.turnState.coverageRate), hint: '（返回 292 B + 返回其他长度）÷（二者 + 无返回值）；历史未采集不计入' },
    ],
  },
  {
    title: '同一次请求的收发关系',
    description: '只比较同一请求、同一 attempt 的实际发送值与实际返回值。',
    items: [
      { label: '收发相同', value: props.summary.turnState.same.toLocaleString('zh-CN') },
      { label: '收发不同', value: props.summary.turnState.different.toLocaleString('zh-CN') },
      { label: '仅返回、未发送', value: props.summary.turnState.onlyReturned.toLocaleString('zh-CN') },
      { label: '收发均无', value: props.summary.turnState.neither.toLocaleString('zh-CN') },
    ],
  },
])
</script>

<template>
  <section v-if="!loading && !error" class="mt-5 grid shrink-0 grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-4" aria-label="使用概览">
    <BaseCard
      v-for="item in items"
      :key="item.key"
      as="article"
      padding="compact"
      class="grid min-h-23 grid-cols-[36px_minmax(0,1fr)] items-stretch gap-3"
    >
      <BaseMotionIcon class="inline-flex size-9 shrink-0 items-center justify-center rounded-cp" :class="item.tone">
        <component :is="item.icon" class="size-4.5" />
      </BaseMotionIcon>
      <div class="flex min-w-0 flex-col justify-between py-0.5">
        <span class="block text-cp-sm leading-none font-bold text-cp-text-quaternary">
          {{ item.label }}
        </span>
        <strong class="block truncate text-[22px] leading-none font-extrabold text-cp-text">
          {{ item.value }}
        </strong>
        <span class="block truncate text-cp-sm leading-none font-emphasis text-cp-text-secondary">
          {{ item.detail }}
        </span>
      </div>
    </BaseCard>
  </section>
  <section class="rounded-cp-card bg-cp-bg-container p-4 shadow-cp-card" :class="loading || error ? 'mt-5' : 'mt-3'" aria-label="请求级 Turn State 统计">
    <h2 class="m-0 text-cp font-heavy text-cp-text">
      请求级 Turn State
    </h2>
    <p class="mt-1 mb-3 text-cp-sm text-cp-text-secondary">
      仅统计筛选范围内已结束的 OpenAI 请求；发送值与返回值独立记录。292 B 是 UTF-8 字节长度的暂定观察规则，不代表模型质量或档位。
    </p>
    <p v-if="loading" role="status" class="m-0 text-cp-sm text-cp-text-secondary">
      正在加载观测统计…
    </p>
    <p v-else-if="error" role="alert" class="m-0 text-cp-sm text-cp-error-text">
      {{ error }}
    </p>
    <div v-else class="grid gap-3 xl:grid-cols-3">
      <section v-for="group in turnStateGroups" :key="group.title" class="min-w-0 rounded-cp border border-cp-border-subtle p-3">
        <h3 class="m-0 text-cp-sm font-heavy text-cp-text">
          {{ group.title }}
        </h3>
        <p class="mt-1 mb-3 text-cp-xs text-cp-text-secondary">
          {{ group.description }}
        </p>
        <div class="grid grid-cols-2 gap-2 sm:grid-cols-3 xl:grid-cols-2 2xl:grid-cols-3">
          <div v-for="item in group.items" :key="item.label" class="min-w-0 rounded-cp bg-cp-fill-quaternary px-3 py-2">
            <div class="text-cp-xs font-bold text-cp-text-secondary">
              {{ item.label }}
            </div>
            <strong class="mt-1 block font-mono text-lg tabular-nums text-cp-text">{{ item.value }}</strong>
            <p v-if="item.hint" class="mt-1 mb-0 text-cp-xs text-cp-text-secondary">
              {{ item.hint }}
            </p>
          </div>
        </div>
      </section>
    </div>
  </section>
</template>
