<script setup lang="ts">
import { ZFormItem, ZSegmented } from '@codex-proxy/ui'
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { applicableOptionalFeatures } from '../utils/optionalFeatures'

const props = withDefaults(defineProps<{
  provider?: string
  authenticationKind?: string
  disabled?: boolean
  preserve?: boolean
}>(), { disabled: false, preserve: false })

const model = defineModel<string[] | undefined>({ required: true })
const { t } = useI18n({ useScope: 'global' })

const features = computed(() => applicableOptionalFeatures(props.provider, props.authenticationKind))
const scopeMode = computed({
  get: () => (model.value === undefined ? 'preserve' : 'configure'),
  set: (value: string) => {
    model.value = value === 'preserve' ? undefined : [...(model.value ?? [])].filter(id => features.value.some(feature => feature.id === id))
  },
})
const scopeOptions = computed(() => [
  { value: 'preserve', label: t('accounts.optionalFeatures.preserve') },
  { value: 'configure', label: t('accounts.optionalFeatures.configure') },
])

function featureMode(id: string) {
  return {
    get: () => (model.value?.includes(id) ? 'off' : 'inherit'),
    set: (value: string) => {
      const current = new Set(model.value ?? [])
      if (value === 'off')
        current.add(id)
      else
        current.delete(id)
      model.value = [...current]
    },
  }
}

function options() {
  return [
    { value: 'inherit', label: t('accounts.optionalFeatures.inherit') },
    { value: 'off', label: t('accounts.optionalFeatures.off') },
  ]
}
</script>

<template>
  <fieldset v-if="features.length" class="m-0 grid min-w-0 gap-3 border-0 p-0">
    <legend class="p-0 text-cp leading-none font-medium text-cp-text-secondary">
      {{ t('accounts.optionalFeatures.label') }}
    </legend>
    <p class="m-0 text-cp-sm leading-5 text-cp-text-tertiary">
      {{ t('accounts.optionalFeatures.help') }}
    </p>
    <ZSegmented
      v-if="preserve"
      v-model="scopeMode"
      :aria-label="t('accounts.optionalFeatures.label')"
      :options="scopeOptions"
      :disabled="disabled"
    />
    <div v-if="!preserve || model !== undefined" class="grid gap-3">
      <ZFormItem v-for="feature in features" :key="feature.id" :label="t(feature.labelKey)">
        <ZSegmented
          :model-value="featureMode(feature.id).get()"
          :aria-label="t(feature.labelKey)"
          :options="options()"
          :disabled="disabled"
          @update:model-value="featureMode(feature.id).set($event)"
        />
      </ZFormItem>
    </div>
  </fieldset>
</template>
