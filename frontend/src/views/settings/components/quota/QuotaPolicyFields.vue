<script setup lang="ts">
import type { QuotaPolicyForm } from './model'
import { ZForm, ZFormItem, ZInput, ZSelect } from '@codex-proxy/ui'

import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { hasAutoReset, policyFromForm } from './model'

defineProps<{ disabled?: boolean }>()
const form = defineModel<QuotaPolicyForm>({ required: true })

const { t } = useI18n({ useScope: 'global' })

const actionOptions = computed(() => (['off', 'stop', 'reset_then_stop'] as const).map(value => ({
  value,
  label: t(`settings.quotaPolicy.actions.${value}`),
})))

const autoReset = computed(() => hasAutoReset(policyFromForm(form.value)))
</script>

<template>
  <div class="grid gap-4">
    <ZForm class="max-w-6xl sm:grid-cols-[8.5rem_minmax(0,1fr)_minmax(0,1fr)] sm:items-end">
      <div class="grid content-center gap-0.5">
        <span class="text-cp-sm font-heavy text-cp-text">{{ t('settings.quotaPolicy.windows.primary') }}</span>
        <span class="text-cp-xs text-cp-text-secondary">{{ t('settings.quotaPolicy.windows.primaryDetail') }}</span>
      </div>
      <ZFormItem :label="t('settings.quotaPolicy.windows.action')">
        <ZSelect
          v-model="form.primaryAction"
          :options="actionOptions"
          :disabled="disabled"
          :aria-label="t('settings.quotaPolicy.windows.actionAria', { window: t('settings.quotaPolicy.windows.primary') })"
        />
      </ZFormItem>
      <ZFormItem :label="t('settings.quotaPolicy.windows.threshold')">
        <ZInput
          v-model="form.primaryThreshold"
          :disabled="disabled || form.primaryAction === 'off'"
          type="number"
          min="1"
          max="100"
          step="1"
          :aria-label="t('settings.quotaPolicy.windows.thresholdAria', { window: t('settings.quotaPolicy.windows.primary') })"
        >
          <template #suffix>
            <span class="text-cp-sm">%</span>
          </template>
        </ZInput>
      </ZFormItem>

      <div class="grid content-center gap-0.5">
        <span class="text-cp-sm font-heavy text-cp-text">{{ t('settings.quotaPolicy.windows.secondary') }}</span>
        <span class="text-cp-xs text-cp-text-secondary">{{ t('settings.quotaPolicy.windows.secondaryDetail') }}</span>
      </div>
      <ZFormItem :label="t('settings.quotaPolicy.windows.action')">
        <ZSelect
          v-model="form.secondaryAction"
          :options="actionOptions"
          :disabled="disabled"
          :aria-label="t('settings.quotaPolicy.windows.actionAria', { window: t('settings.quotaPolicy.windows.secondary') })"
        />
      </ZFormItem>
      <ZFormItem :label="t('settings.quotaPolicy.windows.threshold')">
        <ZInput
          v-model="form.secondaryThreshold"
          :disabled="disabled || form.secondaryAction === 'off'"
          type="number"
          min="1"
          max="100"
          step="1"
          :aria-label="t('settings.quotaPolicy.windows.thresholdAria', { window: t('settings.quotaPolicy.windows.secondary') })"
        >
          <template #suffix>
            <span class="text-cp-sm">%</span>
          </template>
        </ZInput>
      </ZFormItem>
    </ZForm>

    <div v-if="autoReset" class="grid gap-3 rounded-cp-lg bg-cp-warning-container p-4" role="note">
      <p class="m-0 text-cp-sm font-heavy text-cp-warning-on-container">
        {{ t('settings.quotaPolicy.autoReset.title') }}
      </p>
      <ZForm class="sm:grid-cols-2">
        <ZFormItem :label="t('settings.quotaPolicy.autoReset.maxAttempts')" :description="t('settings.quotaPolicy.autoReset.maxAttemptsHelp')">
          <ZInput
            v-model="form.maxAttemptsPer24h"
            :disabled="disabled"
            type="number"
            min="1"
            max="10"
            step="1"
            :aria-label="t('settings.quotaPolicy.autoReset.maxAttempts')"
          >
            <template #suffix>
              <span class="text-cp-sm">{{ t('settings.quotaPolicy.autoReset.times') }}</span>
            </template>
          </ZInput>
        </ZFormItem>
        <ZFormItem :label="t('settings.quotaPolicy.autoReset.cooldown')" :description="t('settings.quotaPolicy.autoReset.cooldownHelp')">
          <ZInput
            v-model="form.cooldownSeconds"
            :disabled="disabled"
            type="number"
            min="3600"
            max="86400"
            step="1"
            :aria-label="t('settings.quotaPolicy.autoReset.cooldown')"
          >
            <template #suffix>
              <span class="text-cp-sm">{{ t('settings.quotaPolicy.autoReset.seconds') }}</span>
            </template>
          </ZInput>
        </ZFormItem>
      </ZForm>
      <p class="m-0 text-cp-xs leading-relaxed text-cp-warning-on-container">
        {{ t('settings.quotaPolicy.autoReset.boundary') }}
      </p>
    </div>

    <div class="grid gap-1 text-cp-xs leading-relaxed text-cp-text-secondary">
      <p class="m-0">
        {{ t('settings.quotaPolicy.footnotes.stopOnly') }}
      </p>
      <p class="m-0">
        {{ t('settings.quotaPolicy.footnotes.ratioOnly') }}
      </p>
    </div>
  </div>
</template>
