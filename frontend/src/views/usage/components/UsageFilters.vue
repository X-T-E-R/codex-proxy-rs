<script setup lang="ts">
import type { UsageRecordFilterField } from '../composables/useUsageRecordsTable'
import type { UsageErrorType } from '@/api'
import { ZIconButton, ZInput, ZSelect } from '@codex-proxy/ui'

import { Check, RefreshCw, Search } from '@lucide/vue'

import { computed } from 'vue'
import { useI18n } from 'vue-i18n'

defineProps<{
  refreshing: boolean
  loading: boolean
}>()

const emit = defineEmits<{
  refresh: []
}>()

const { t } = useI18n({ useScope: 'global' })

const field = defineModel<UsageRecordFilterField>('field', { required: true })
const value = defineModel<string>('value', { required: true })
const errorTypes = defineModel<UsageErrorType[]>('errorTypes', { default: () => [] })
const fieldOptions = computed(() => [
  { value: 'search', label: t('usage.filters.search') },
  { value: 'key', label: t('usage.filters.keyName') },
  { value: 'account', label: t('usage.filters.account') },
  { value: 'model', label: t('usage.fields.model') },
  { value: 'error', label: t('usage.filters.error') },
  { value: 'requestId', label: t('usage.filters.requestId') },
  { value: 'upstreamRequestId', label: t('usage.filters.upstreamRequestId') },
  { value: 'responseId', label: t('usage.filters.responseId') },
])
const errorTypeOptions = computed<{ value: UsageErrorType, label: string, description: string }[]>(() => [
  { value: 'overloaded', label: t('usage.filters.errorTypes.overloaded'), description: 'server_overloaded' },
  { value: 'status_503', label: t('usage.filters.errorTypes.status503'), description: '503' },
  { value: 'rate_limited', label: t('usage.filters.errorTypes.rateLimited'), description: 'rate_limited · 429' },
  { value: 'upstream_unavailable', label: t('usage.filters.errorTypes.upstreamUnavailable'), description: 'upstream_unavailable · 5xx' },
  { value: 'timeout', label: t('usage.filters.errorTypes.timeout'), description: 'timeout' },
  { value: 'no_available_provider', label: t('usage.filters.errorTypes.noAvailableProvider'), description: 'no_available_provider' },
  { value: 'capacity_unavailable', label: t('usage.filters.errorTypes.capacityUnavailable'), description: 'account_capacity_unavailable' },
  { value: 'provider_infrastructure_unavailable', label: t('usage.filters.errorTypes.providerInfrastructureUnavailable'), description: 'provider_infrastructure_unavailable' },
  { value: 'unauthorized', label: t('usage.filters.errorTypes.unauthorized'), description: 'unauthorized · 401/403' },
  { value: 'continuation_recovery_required', label: t('usage.filters.errorTypes.continuationRecoveryRequired'), description: 'continuation_recovery_required' },
  { value: 'invalid_request', label: t('usage.filters.errorTypes.invalidRequest'), description: 'invalid_request · 400' },
  { value: 'process_interrupted', label: t('usage.filters.errorTypes.processInterrupted'), description: 'process_interrupted' },
])
</script>

<template>
  <div class="flex w-full flex-wrap items-center gap-3" role="group" :aria-label="t('usage.filters.label')">
    <div class="grid w-full min-w-0 grid-cols-[9.5rem_minmax(0,1fr)] gap-2 sm:w-md sm:flex-none">
      <ZSelect v-model="field" :options="fieldOptions" :aria-label="t('usage.filters.fieldLabel')" />
      <ZInput
        v-model="value"
        :placeholder="t('usage.filters.enterFilterValue')"
        :aria-label="t('usage.filters.valueLabel')"
        class="w-full"
      >
        <template #prefix>
          <Search class="size-4.5 text-cp-text-tertiary" />
        </template>
      </ZInput>
    </div>

    <ZSelect
      v-model="errorTypes"
      :options="errorTypeOptions"
      multiple
      clearable
      filterable
      collapse-tags
      collapse-tags-tooltip
      :max-collapse-tags="1"
      :placeholder="t('usage.filters.errorTypePlaceholder')"
      :aria-label="t('usage.filters.errorType')"
      class="w-full min-w-0 sm:w-72 sm:flex-none"
    >
      <template #option="{ option, selected }">
        <span
          aria-hidden="true"
          class="inline-flex size-4 shrink-0 items-center justify-center rounded-cp-xs transition-colors"
          :class="selected ? 'bg-cp-primary text-cp-text-light-solid' : 'bg-(--cp-input-bg) shadow-cp-input'"
        >
          <Check v-if="selected" :size="12" :stroke-width="2.5" />
        </span>
        <span class="flex min-w-0 flex-1 flex-col gap-0.5">
          <span class="whitespace-normal">{{ option.label }}</span>
          <span
            v-if="option.description"
            :title="option.description"
            class="truncate font-mono text-cp-xs font-normal text-cp-text-tertiary"
          >
            {{ option.description }}
          </span>
        </span>
      </template>
    </ZSelect>

    <div class="ml-auto flex shrink-0 items-center justify-end gap-2">
      <slot name="actions" />
      <ZIconButton
        size="default"
        :aria-label="t('usage.filters.refreshUsageHistory')"
        :loading="refreshing"
        :disabled="loading || refreshing"
        @click="emit('refresh')"
      >
        <template #loading>
          <RefreshCw class="size-4.5 animate-spin motion-reduce:animate-none" />
        </template>
        <RefreshCw class="size-4.5" />
      </ZIconButton>
    </div>
  </div>
</template>
