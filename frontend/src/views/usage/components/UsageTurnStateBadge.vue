<script setup lang="ts">
import type { UsageTurnStateSummary } from '@/api/modules/usage'
import { computed } from 'vue'

const props = defineProps<{ state: UsageTurnStateSummary }>()

const label = computed(() => {
  const relation = {
    'same': '收发相同',
    'different': '收发不同',
    'only-sent': '仅发送',
    'only-returned': '仅返回',
    'neither': '均无',
    'unknown': '发送未知',
  }[props.state.relation]
  const sent = props.state.sentBytes === null ? '发 —' : `发 ${props.state.sentBytes} B`
  const returned = props.state.returnedBytes === null ? '回 —' : `回 ${props.state.returnedBytes} B`
  return `${sent} / ${returned} · ${relation ?? '关系未知'}`
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
    :title="`实际发送给上游：${state.sentBytes ?? '—'} B；上游实际返回：${state.returnedBytes ?? '—'} B；${label}`"
  >
    {{ label }}
  </span>
</template>
