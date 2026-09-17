<script setup lang="ts">
import type { UsageTurnStateDetail } from '@/api/modules/usage'
import { Copy, Eye, EyeOff } from '@lucide/vue'
import { ref, watch } from 'vue'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import { useCopyText } from '@/composables/useCopyText'
import { formatDateTime } from '@/utils/date'
import UsageTurnStateBadge from './UsageTurnStateBadge.vue'

const props = defineProps<{ state: UsageTurnStateDetail }>()
const shown = ref(false)
const copyText = useCopyText()

watch(() => props.state, () => {
  shown.value = false
})

const descriptions: Record<UsageTurnStateDetail['classification'], string> = {
  observed292: '上游返回的值按 UTF-8 计为 292 字节，属于暂定观察命中；不证明模型质量或档位。',
  observedOther: '已观测到上游值，但 UTF-8 字节长度不是 292。',
  unobserved: '这次 OpenAI 请求结束时没有观测到上游 Turn State。',
  notCollected: '这条历史记录创建时尚未采集请求级 Turn State，无法回溯本次的值。',
  pending: '这次 OpenAI 请求仍在进行，观测结果尚未确定。',
  notApplicable: '这次请求不属于 OpenAI 请求级 Turn State 采集范围。',
}
</script>

<template>
  <section class="grid min-w-0 gap-3 rounded-cp-card bg-cp-fill-quaternary px-4 py-3.5" aria-label="请求级 Turn State">
    <div class="flex flex-wrap items-center gap-2">
      <h3 class="m-0 text-cp-sm font-heavy text-cp-text-secondary">
        请求级 Turn State
      </h3>
      <UsageTurnStateBadge :state="state" />
    </div>
    <p class="m-0 text-cp-sm text-cp-text-secondary">
      {{ descriptions[state.classification] ?? '本次请求的 Turn State 分类未知。' }}
    </p>
    <div v-if="state.value !== null" class="flex min-w-0 flex-wrap items-center gap-2">
      <BaseInput :model-value="state.value" :type="shown ? 'text' : 'password'" readonly autocomplete="off" aria-label="本次请求观测值" class="min-w-40 flex-1" />
      <BaseButton size="sm" :aria-label="shown ? '隐藏本次请求观测值' : '显示本次请求观测值'" @click="shown = !shown">
        <template #icon>
          <EyeOff v-if="shown" :size="14" /><Eye v-else :size="14" />
        </template>
        {{ shown ? '隐藏' : '显示' }}
      </BaseButton>
      <BaseButton size="sm" @click="copyText(state.value!, { successText: '本次请求观测值已复制' })">
        <template #icon>
          <Copy :size="14" />
        </template>
        复制
      </BaseButton>
    </div>
    <dl class="m-0 grid min-w-0 gap-x-4 gap-y-3 text-cp-sm sm:grid-cols-2 lg:grid-cols-3">
      <div
        v-for="item in [
          { label: '长度', value: state.bytes === null ? '未提供' : `${state.bytes} 字节` },
          { label: '来源', value: state.source === 'websocket' ? 'WebSocket' : state.source === 'http' ? 'HTTP' : '未提供' },
          { label: '观测时间', value: state.observedAt ? formatDateTime(state.observedAt) : '未提供' },
          { label: '尝试序号', value: state.attemptIndex ?? '未提供' },
          { label: '上游响应 ID', value: state.upstreamResponseId ?? '未提供' },
          { label: 'SHA-256', value: state.sha256 ?? '未提供' },
          { label: '变化标记', value: state.value === null ? '未提供' : state.changed ? '是' : '否' },
        ]" :key="item.label" class="min-w-0"
      >
        <dt class="text-cp-text-quaternary">
          {{ item.label }}
        </dt>
        <dd class="m-0 break-all font-mono text-cp-text">
          {{ item.value }}
        </dd>
      </div>
    </dl>
  </section>
</template>
