<script setup lang="ts">
import type { UsageTurnStateSummary } from '@/api/modules/usage'
import { computed } from 'vue'

const props = defineProps<{ state: UsageTurnStateSummary }>()

const label = computed(() => {
  switch (props.state.classification) {
    case 'observed292': return '292 B 命中'
    case 'observedOther': return props.state.bytes === null ? '其他长度（长度未提供）' : `${props.state.bytes} B 其他长度`
    case 'unobserved': return '未观测'
    case 'notCollected': return '历史未采集'
    case 'pending': return '采集中'
    case 'notApplicable': return '不适用'
    default: return '未知'
  }
})
const tone = computed(() => {
  switch (props.state.classification) {
    case 'observed292': return 'bg-cp-green-container text-cp-green-on-container'
    case 'observedOther': return 'bg-cp-orange-container text-cp-orange-on-container'
    case 'unobserved': return 'bg-cp-fill-tertiary text-cp-text-secondary'
    default: return 'bg-cp-fill-quaternary text-cp-text-tertiary'
  }
})
</script>

<template>
  <span
    class="inline-flex max-w-full items-center rounded-cp px-2 py-1 text-cp-xs leading-none font-bold whitespace-nowrap"
    :class="tone"
    :title="state.classification === 'observed292' ? '按 UTF-8 字节长度暂定命中 292 B；不代表模型质量或档位' : label"
  >
    {{ label }}
  </span>
</template>
