<script setup lang="ts">
import type { ReasoningEffortMode, ReasoningEffortValue, ServiceTierRule } from '@/api'
import { GitBranch, Plus, Trash2 } from '@lucide/vue'

import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseIconButton from '@/components/base/BaseIconButton.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'

export interface ModelPolicyRow {
  model: string
  effortMode: ReasoningEffortMode | ''
  effortValue: ReasoningEffortValue | ''
  serviceTier: ServiceTierRule | ''
}

withDefaults(defineProps<{
  rows: ModelPolicyRow[]
  loading?: boolean
  error?: string
}>(), {
  loading: false,
  error: '',
})

const emit = defineEmits<{
  addRow: []
  updateRow: [index: number, key: keyof ModelPolicyRow, value: string]
  removeRow: [index: number]
}>()

const effortModeOptions = [
  { label: '不干预', value: '' },
  { label: '锁定', value: 'locked' },
  { label: '最小', value: 'min' },
  { label: '最大', value: 'max' },
]

const effortValueOptions = [
  { label: 'none', value: 'none' },
  { label: 'minimal', value: 'minimal' },
  { label: 'low', value: 'low' },
  { label: 'medium', value: 'medium' },
  { label: 'high', value: 'high' },
  { label: 'xhigh', value: 'xhigh' },
  { label: 'max', value: 'max' },
]

const serviceTierOptions = [
  { label: '不干预', value: '' },
  { label: '锁定 fast', value: 'lock_fast' },
  { label: '永不 fast', value: 'lock_never_fast' },
]
</script>

<template>
  <BaseCard
    title="模型请求策略"
    description="按客户端请求的模型名精确匹配，在路由到 Provider 前改写推理强度与 service tier"
  >
    <p class="mb-4 text-cp-sm text-cp-text-secondary">
      推理强度「锁定」无条件改写，「最小」仅在请求缺失或低于目标时提升，「最大」仅在请求缺失或高于目标时降为
      目标。service tier 仅对 OpenAI 路由模型生效；强度模式与 service tier 至少配置一项，
      两项都不干预的行不会保存。
    </p>
    <div class="grid gap-4">
      <div class="flex flex-wrap items-center gap-3">
        <BaseButton variant="secondary" :disabled="loading" @click="emit('addRow')">
          <template #icon>
            <Plus class="size-4" />
          </template>
          添加策略
        </BaseButton>
        <span v-if="error" class="text-xs font-emphasis text-cp-error-text">{{ error }}</span>
      </div>

      <div class="flex items-center gap-2 text-cp-sm font-emphasis text-cp-text-secondary">
        <GitBranch class="size-4 text-cp-primary-text" />
        模型策略
      </div>

      <div
        v-if="loading"
        class="rounded-cp bg-cp-fill-quaternary px-4 py-4 text-cp font-emphasis text-cp-text-quaternary"
      >
        正在加载模型策略...
      </div>
      <div
        v-else-if="rows.length === 0"
        class="rounded-cp bg-cp-fill-quaternary px-4 py-4 text-cp font-emphasis text-cp-text-quaternary"
      >
        暂无模型请求策略
      </div>
      <div v-else class="grid gap-3">
        <div
          v-for="(row, index) in rows"
          :key="index"
          class="grid items-center gap-3 rounded-cp-card bg-cp-fill-quaternary p-3 lg:grid-cols-[1.2fr_1fr_1fr_1.1fr_auto]"
        >
          <BaseInput
            :model-value="row.model"
            placeholder="模型名"
            aria-label="模型名"
            @update:model-value="emit('updateRow', index, 'model', $event)"
          />
          <BaseSelect
            :model-value="row.effortMode"
            :options="effortModeOptions"
            aria-label="推理强度模式"
            :disabled="loading"
            @update:model-value="emit('updateRow', index, 'effortMode', $event)"
          />
          <BaseSelect
            :model-value="row.effortValue"
            :options="effortValueOptions"
            aria-label="推理强度档位"
            :disabled="loading || !row.effortMode"
            @update:model-value="emit('updateRow', index, 'effortValue', $event)"
          />
          <BaseSelect
            :model-value="row.serviceTier"
            :options="serviceTierOptions"
            aria-label="service tier 策略"
            :disabled="loading"
            @update:model-value="emit('updateRow', index, 'serviceTier', $event)"
          />
          <BaseIconButton
            variant="ghost"
            label="删除策略"
            @click="emit('removeRow', index)"
          >
            <Trash2 class="size-4 text-cp-error" />
          </BaseIconButton>
        </div>
      </div>
    </div>
  </BaseCard>
</template>
