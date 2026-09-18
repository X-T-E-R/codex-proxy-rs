<script setup lang="ts">
import type { UsageTurnStateDetail, UsageTurnStateEvidence } from '@/api/modules/usage'
import { Copy, Eye, EyeOff } from '@lucide/vue'
import { reactive, watch } from 'vue'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import { useCopyText } from '@/composables/useCopyText'
import { formatDateTime } from '@/utils/date'
import UsageTurnStateBadge from './UsageTurnStateBadge.vue'

const props = defineProps<{ state: UsageTurnStateDetail }>()
const shown = reactive({ sent: false, returned: false })
const copyText = useCopyText()

watch(() => props.state, () => {
  shown.sent = false
  shown.returned = false
})

const relationLabels: Record<UsageTurnStateDetail['relation'], string> = {
  'same': '发送值与返回值相同',
  'different': '发送值与返回值不同',
  'only-sent': '仅确认发送值',
  'only-returned': '仅收到上游返回值',
  'neither': '本次没有 Turn State',
  'unknown': '历史发送证据未知',
}

function fields(evidence: UsageTurnStateEvidence) {
  return [
    { label: '长度', value: `${evidence.bytes} 字节` },
    { label: 'Transport', value: evidence.transport === 'websocket' ? 'WebSocket' : 'HTTP' },
    { label: '时间', value: formatDateTime(evidence.timestamp) },
    { label: '来源', value: evidence.source },
    { label: '账号', value: evidence.accountId ?? '未提供' },
    { label: '身份代次', value: evidence.identityRevision ?? '未提供' },
    { label: '实际模型', value: evidence.effectiveModel ?? '未提供' },
    { label: '值代次', value: evidence.generation ?? '未提供' },
    { label: 'Candidate ID', value: evidence.candidateId ?? '未提供' },
    { label: 'Token version', value: evidence.tokenVersion === null ? '未识别' : `0x${evidence.tokenVersion.toString(16).padStart(2, '0')}` },
    { label: '内嵌时间（未验签）', value: evidence.issuedAt ? formatDateTime(evidence.issuedAt) : '未识别' },
    { label: '上游响应 ID', value: evidence.upstreamResponseId ?? '未提供' },
    { label: 'SHA-256', value: evidence.sha256 },
    { label: '流中变化', value: evidence.changed ? '是' : '否' },
  ]
}
</script>

<template>
  <section class="grid min-w-0 gap-4 rounded-cp-card bg-cp-fill-quaternary px-4 py-3.5" aria-label="请求级 Turn State">
    <div class="flex flex-wrap items-center gap-2">
      <h3 class="m-0 text-cp-sm font-heavy text-cp-text-secondary">
        请求级 Turn State
      </h3>
      <UsageTurnStateBadge :state="state" />
      <span class="rounded-cp bg-cp-bg-container px-2 py-1 text-cp-xs font-bold text-cp-text-secondary">
        {{ relationLabels[state.relation] ?? '关系未知' }}
      </span>
    </div>

    <article v-for="side in (['sent', 'returned'] as const)" :key="side" class="grid gap-3 rounded-cp bg-cp-bg-container p-4">
      <h4 class="m-0 font-bold text-cp-text">
        {{ side === 'sent' ? '实际发送给上游' : '上游实际返回' }}
      </h4>
      <template v-if="state[side]">
        <div class="flex min-w-0 flex-wrap items-center gap-2">
          <BaseInput :model-value="state[side]!.value" :type="shown[side] ? 'text' : 'password'" readonly autocomplete="off" :aria-label="side === 'sent' ? '发送 Turn State' : '返回 Turn State'" class="min-w-40 flex-1" />
          <BaseButton size="sm" @click="shown[side] = !shown[side]">
            <template #icon>
              <EyeOff v-if="shown[side]" :size="14" /><Eye v-else :size="14" />
            </template>
            {{ shown[side] ? '隐藏' : '显示' }}
          </BaseButton>
          <BaseButton size="sm" @click="copyText(state[side]!.value, { successText: 'Turn State 已复制' })">
            <template #icon>
              <Copy :size="14" />
            </template>
            复制
          </BaseButton>
        </div>
        <dl class="m-0 grid min-w-0 gap-x-4 gap-y-3 text-cp-sm sm:grid-cols-2 lg:grid-cols-3">
          <div v-for="item in fields(state[side]!)" :key="item.label" class="min-w-0">
            <dt class="text-cp-text-quaternary">
              {{ item.label }}
            </dt>
            <dd class="m-0 break-all font-mono text-cp-text">
              {{ item.value }}
            </dd>
          </div>
        </dl>
      </template>
      <p v-else class="m-0 text-cp-sm text-cp-text-secondary">
        {{ side === 'sent' && state.relation === 'unknown' ? '历史记录没有可靠发送证据。' : '本次没有保存这一方向的值。' }}
      </p>
    </article>
  </section>
</template>
