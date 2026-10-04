<script setup lang="ts">
import type { AccountQuotaPolicy } from '@/api'
import { formatDateTime } from '@/utils/date'
import { quotaPolicyReason, quotaPolicySource, quotaPolicySummary } from './model'

defineProps<{ value: AccountQuotaPolicy }>()
</script>

<template>
  <div class="grid min-w-0 gap-2 text-cp-xs" aria-label="额度策略状态">
    <div class="flex flex-wrap items-center gap-2">
      <span class="font-emphasis text-cp-text-secondary">有效来源：{{ quotaPolicySource(value.source) }}</span>
      <strong
        class="rounded-cp-sm px-2 py-1"
        :class="value.status.paused || value.status.pendingOperationId ? 'bg-cp-warning-container text-cp-warning-on-container' : 'bg-cp-fill-tertiary text-cp-text-secondary'"
      >
        {{ value.status.pendingOperationId ? '重置待确认' : value.status.paused ? '已暂停新分配' : '未被额度策略暂停' }}
      </strong>
    </div>
    <p class="m-0 leading-relaxed text-cp-text-secondary">
      {{ quotaPolicySummary(value.effectivePolicy) }}
    </p>
    <p class="m-0 leading-relaxed" :class="value.status.paused || value.status.pendingOperationId ? 'text-cp-warning-text' : 'text-cp-text-secondary'" role="status">
      {{ quotaPolicyReason(value.status.reason) }}
    </p>
    <p v-if="value.status.pendingOperationId && value.status.reason !== 'reset_pending'" class="m-0 leading-relaxed text-cp-warning-text">
      上次重置结果待确认，确认前不会消耗下一张卡
    </p>
    <details class="text-cp-text-secondary">
      <summary class="w-fit cursor-pointer rounded-cp-sm py-1 outline-none focus-visible:ring-2 focus-visible:ring-cp-control-outline">
        自动重置记录与限制
      </summary>
      <dl class="mt-2 mb-0 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-2">
        <dt>滚动 24 小时自动发送</dt>
        <dd class="m-0 font-mono tabular-nums">
          {{ value.status.autoAttemptsLast24h }} / {{ value.effectivePolicy.autoReset.maxAttemptsPer24h }} 次
        </dd>
        <dt>最小间隔</dt>
        <dd class="m-0 font-mono tabular-nums">
          {{ value.effectivePolicy.autoReset.cooldownSeconds }} 秒
        </dd>
        <dt>上次自动发送</dt>
        <dd class="m-0 wrap-anywhere">
          {{ value.status.lastAutoAttemptAt ? formatDateTime(value.status.lastAutoAttemptAt, '时间未知') : '尚无记录' }}
        </dd>
        <dt>策略观测时间</dt>
        <dd class="m-0 wrap-anywhere">
          {{ value.status.observedAt ? formatDateTime(value.status.observedAt, '时间未知') : '尚未观测' }}
        </dd>
      </dl>
    </details>
  </div>
</template>
