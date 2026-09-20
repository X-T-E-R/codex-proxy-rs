<script setup lang="ts">
import BaseCheckbox from '@/components/base/BaseCheckbox.vue'

withDefaults(defineProps<{
  models: Array<{ id: string, label: string }>
  loading?: boolean
  disabled?: boolean
}>(), { loading: false, disabled: false })

const selectedModels = defineModel<string[]>({ required: true })

function update(modelId: string, selected: boolean) {
  const next = new Set(selectedModels.value)
  if (selected)
    next.add(modelId)
  else next.delete(modelId)
  selectedModels.value = [...next]
}
</script>

<template>
  <div class="grid gap-2">
    <p class="m-0 text-cp-sm text-cp-text-secondary">
      未勾选任何模型时允许全部模型；勾选后，该账号只参与所选模型的调度。
    </p>
    <div v-if="models.length" class="grid max-h-52 gap-2 overflow-y-auto sm:grid-cols-2">
      <div
        v-for="model in models"
        :key="model.id"
        class="flex min-h-11 items-center rounded-cp bg-cp-fill-quaternary px-3.5 py-2.5"
      >
        <BaseCheckbox
          :model-value="selectedModels.includes(model.id)"
          :label="model.label"
          show-label
          :disabled="disabled || loading"
          @update:model-value="update(model.id, $event)"
        />
      </div>
    </div>
    <p v-else class="m-0 rounded-cp bg-cp-fill-quaternary px-3.5 py-3 text-cp-sm font-emphasis text-cp-text-secondary">
      {{ loading ? '正在加载账号模型...' : '账号目录暂无模型；保持空选将允许全部模型。' }}
    </p>
  </div>
</template>
