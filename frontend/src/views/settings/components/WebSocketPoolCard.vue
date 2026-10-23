<script setup lang="ts">
import { ZCard, ZForm, ZFormItem, ZInput, ZSwitch } from '@codex-proxy/ui'

import { Gauge, Timer, Zap } from '@lucide/vue'

import { useI18n } from 'vue-i18n'

const { t } = useI18n({ useScope: 'global' })

const enabled = defineModel<boolean>('enabled', { required: true })
const maxAgeMs = defineModel<string>('maxAgeMs', { required: true })
const maxConnecting = defineModel<string>('maxConnecting', { required: true })
const streamIdleTimeoutMs = defineModel<string>('streamIdleTimeoutMs', { required: true })
const fastPathBudgetMs = defineModel<string>('fastPathBudgetMs', { required: true })
</script>

<template>
  <ZCard :title="t('settings.wsPool.title')" :description="t('settings.wsPool.description')">
    <ZForm class="max-w-6xl sm:grid-cols-2 sm:items-end">
      <div class="col-span-full">
        <ZSwitch v-model="enabled" :label="t('settings.wsPool.enable')" />
        <p class="m-0 mt-1.5 text-cp-sm leading-relaxed text-cp-text-secondary">
          {{ t('settings.wsPool.enableHelp') }}
        </p>
      </div>

      <ZFormItem :label="t('settings.wsPool.maxAge')" :description="t('settings.wsPool.maxAgeHelp')">
        <ZInput
          v-model="maxAgeMs"
          :disabled="!enabled"
          :aria-label="t('settings.wsPool.maxAge')"
          type="number"
          min="1"
          step="1"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
          <template #suffix>
            <span class="text-cp-sm">{{ t('settings.wsPool.milliseconds') }}</span>
          </template>
        </ZInput>
      </ZFormItem>

      <ZFormItem :label="t('settings.wsPool.maxConnecting')" :description="t('settings.wsPool.maxConnectingHelp')">
        <ZInput
          v-model="maxConnecting"
          :disabled="!enabled"
          :aria-label="t('settings.wsPool.maxConnecting')"
          type="number"
          min="1"
          max="4294967295"
          step="1"
        >
          <template #prefix>
            <Gauge class="size-4" />
          </template>
          <template #suffix>
            <span class="text-cp-sm">{{ t('settings.wsPool.connections') }}</span>
          </template>
        </ZInput>
      </ZFormItem>

      <ZFormItem :label="t('settings.wsPool.streamIdleTimeout')" :description="t('settings.wsPool.streamIdleTimeoutHelp')">
        <ZInput
          v-model="streamIdleTimeoutMs"
          :disabled="!enabled"
          :aria-label="t('settings.wsPool.streamIdleTimeout')"
          type="number"
          min="1"
          step="1"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
          <template #suffix>
            <span class="text-cp-sm">{{ t('settings.wsPool.milliseconds') }}</span>
          </template>
        </ZInput>
      </ZFormItem>

      <ZFormItem :label="t('settings.wsPool.fastPathBudget')" :description="t('settings.wsPool.fastPathBudgetHelp')">
        <ZInput
          v-model="fastPathBudgetMs"
          :disabled="!enabled"
          :aria-label="t('settings.wsPool.fastPathBudget')"
          type="number"
          min="1"
          step="1"
        >
          <template #prefix>
            <Zap class="size-4" />
          </template>
          <template #suffix>
            <span class="text-cp-sm">{{ t('settings.wsPool.milliseconds') }}</span>
          </template>
        </ZInput>
      </ZFormItem>
    </ZForm>
  </ZCard>
</template>
