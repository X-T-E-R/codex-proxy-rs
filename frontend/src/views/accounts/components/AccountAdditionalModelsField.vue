<script setup lang="ts">
import { ZButton, ZFormItem, ZIconButton, ZInput } from '@codex-proxy/ui'
import { Plus, X } from '@lucide/vue'
import { ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { accountModelIdError } from '../utils/modelAccess'

const props = defineProps<{ disabled?: boolean }>()
const { t } = useI18n({ useScope: 'global' })
const models = defineModel<string[]>({ required: true })
const input = ref('')
const error = ref('')

function add() {
  if (props.disabled)
    return
  const id = input.value.trim()
  error.value = accountModelIdError(id) ?? ''
  if (!error.value && models.value.length >= 256)
    error.value = t('accounts.modelAccess.configureUpToModels')
  if (!error.value && models.value.includes(id))
    error.value = t('accounts.additionalModels.duplicate')
  if (error.value)
    return
  models.value = [...models.value, id]
  input.value = ''
}

function remove(id: string) {
  if (!props.disabled)
    models.value = models.value.filter(model => model !== id)
}

watch(input, () => {
  error.value = ''
})
</script>

<template>
  <ZFormItem :label="t('accounts.additionalModels.title')">
    <div class="grid gap-2">
      <p class="m-0 text-cp-xs text-cp-text-tertiary">
        {{ t('accounts.additionalModels.help') }}
      </p>
      <div class="flex items-center gap-2">
        <ZInput v-model="input" :aria-label="t('accounts.additionalModels.input')" :placeholder="t('accounts.additionalModels.placeholder')" :aria-invalid="Boolean(error)" :disabled="disabled" class="min-w-0 flex-1" @keydown.enter.prevent="add" />
        <ZButton :disabled="disabled || !input.trim()" @click="add">
          <Plus class="size-4" aria-hidden="true" />
          {{ t('accounts.additionalModels.add') }}
        </ZButton>
      </div>
      <p v-if="error" class="m-0 text-cp-xs text-cp-error-text" role="alert">
        {{ error }}
      </p>
      <div v-if="models.length" class="flex flex-wrap gap-2" :aria-label="t('accounts.additionalModels.title')">
        <div v-for="id in models" :key="id" class="flex min-w-0 max-w-full items-center gap-1 rounded-cp bg-cp-fill-quaternary py-1 pl-2.5 pr-1 text-cp-sm text-cp-text">
          <span class="min-w-0 truncate" :title="id">{{ id }}</span>
          <ZIconButton :aria-label="t('accounts.additionalModels.remove', { id })" size="small" :disabled="disabled" @click="remove(id)">
            <X class="size-3.5" aria-hidden="true" />
          </ZIconButton>
        </div>
      </div>
    </div>
  </ZFormItem>
</template>
