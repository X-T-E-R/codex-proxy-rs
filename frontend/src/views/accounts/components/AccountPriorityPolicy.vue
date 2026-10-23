<script setup lang="ts">
import type { Account, FastMode } from '@/api'

import { ZButton, ZEmpty, ZIconButton, ZPopover, ZSegmented, ZSkeleton } from '@codex-proxy/ui'

import { CircleAlert, RefreshCw, Save, TriangleAlert, Undo2 } from '@lucide/vue'

import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { useAccountPriorityPolicy } from '../composables/useAccountPriorityPolicy'

const props = defineProps<{ account: Account }>()

const { t } = useI18n({ useScope: 'global' })

const {
  policy,
  mode,
  autoOffOnQuotaReset,
  loading,
  saving,
  loadError,
  conflict,
  dirty,
  effectiveMode,
  effectiveAutoOffOnQuotaReset,
  status,
  load,
  save,
  reset,
} = useAccountPriorityPolicy({ accountId: () => props.account.id })

const modeOptions = computed(() => [
  { label: t('accounts.priority.modes.inherit'), value: 'inherit' },
  { label: t('common.status.enabled'), value: 'enabled' },
  { label: t('common.status.disabled'), value: 'disabled' },
])

// 自动关闭与档位一致使用三态：继承全局、显式开启、显式关闭
const autoOffOptions = computed(() => [
  { label: t('accounts.priority.modes.inherit'), value: 'inherit' },
  { label: t('common.values.on'), value: 'on' },
  { label: t('common.values.off'), value: 'off' },
])
const autoOffChoice = computed({
  get: () => autoOffOnQuotaReset.value === null ? 'inherit' : autoOffOnQuotaReset.value ? 'on' : 'off',
  set: (value: string) => {
    autoOffOnQuotaReset.value = value === 'inherit' ? null : value === 'on'
  },
})

function modeLabel(mode: FastMode) {
  switch (mode) {
    case 'enabled': return t('common.status.enabled')
    case 'disabled': return t('common.status.disabled')
    default: return t('common.values.default')
  }
}

const effectiveLabel = computed(() => modeLabel(effectiveMode.value))
const autoOffLabel = computed(() => effectiveAutoOffOnQuotaReset.value
  ? t('common.values.on')
  : t('common.values.off'))

const statusText = computed(() => {
  switch (status.value?.reason) {
    case 'awaiting_baseline': return t('accounts.priority.status.awaitingBaseline')
    case 'armed': return t('accounts.priority.status.armed')
    case 'quota_reset': return t('accounts.priority.status.quotaReset')
    default: return ''
  }
})
// 自动关闭是后台写入的结果，命中时用警示色提示当前档位已被改动
const statusTone = computed(() => {
  if (status.value?.reason === 'quota_reset')
    return 'text-cp-warning'
  return status.value?.reason === 'armed' ? 'text-cp-success-text' : 'text-cp-text-secondary'
})
</script>

<template>
  <section
    class="rounded-lg bg-cp-bg-container p-4 shadow-cp-tertiary lg:col-span-2"
    :aria-label="t('accounts.priority.title')"
  >
    <div class="flex flex-wrap items-start gap-3">
      <div class="min-w-0">
        <div class="flex items-center gap-1">
          <h3 class="m-0 text-cp-lg font-heavy text-cp-text">
            {{ t('accounts.priority.title') }}
          </h3>
          <ZPopover class="-my-1" trigger="hover-click" placement="top-start">
            <template #reference="{ open }">
              <ZIconButton
                :aria-label="t('accounts.priority.help.label')"
                :title="undefined"
                :aria-expanded="open"
                class="size-6! hover:bg-transparent! active:bg-transparent!"
              >
                <CircleAlert class="size-3.5" aria-hidden="true" />
              </ZIconButton>
            </template>
            <div class="grid max-w-72 gap-2 px-3 py-2 text-cp-sm leading-relaxed text-cp-text-secondary">
              <p>{{ t('accounts.priority.help.baseline') }}</p>
              <p>{{ t('accounts.priority.help.inherit') }}</p>
              <p>{{ t('accounts.priority.help.percent') }}</p>
            </div>
          </ZPopover>
          <ZIconButton
            v-if="policy"
            :aria-label="t('accounts.priority.refresh')"
            :loading="loading"
            :disabled="saving"
            @click="load(dirty)"
          >
            <template #loading>
              <RefreshCw class="size-3.5 animate-spin motion-reduce:animate-none" />
            </template>
            <RefreshCw class="size-3.5" />
          </ZIconButton>
        </div>
        <p v-if="policy" class="m-0 mt-1 text-cp-xs font-emphasis text-cp-text-secondary">
          {{ t('accounts.priority.effective', { mode: effectiveLabel }) }}
          <span aria-hidden="true">·</span>
          {{ t('accounts.priority.effectiveAutoOff', { state: autoOffLabel }) }}
        </p>
      </div>
    </div>

    <div v-if="loading && !policy" class="mt-3 grid gap-4 sm:grid-cols-[16rem_16rem]" role="status" aria-busy="true">
      <span class="sr-only">{{ t('common.status.loading') }}</span>
      <div v-for="field in 2" :key="field" class="grid w-64 gap-1.5">
        <ZSkeleton shape="text" class="w-14" />
        <ZSkeleton class="h-cp-control w-full" />
      </div>
    </div>

    <ZEmpty
      v-else-if="loadError"
      :title="t('accounts.priority.loadFailed')"
      :description="loadError"
      :icon="TriangleAlert"
      size="small"
      surface="none"
      role="alert"
    >
      <template #action>
        <ZButton size="small" variant="plain" :loading="loading" @click="load()">
          {{ t('common.actions.retry') }}
        </ZButton>
      </template>
    </ZEmpty>

    <template v-else-if="policy">
      <p v-if="conflict" role="status" class="m-0 mt-2 text-cp-xs font-emphasis text-cp-warning">
        {{ t('accounts.priority.conflict') }}
      </p>
      <div class="mt-3 grid gap-4 sm:grid-cols-[16rem_16rem_auto] sm:items-end">
        <div class="grid w-64 gap-1.5">
          <span class="text-[10px] font-bold text-cp-text-quaternary">
            {{ t('accounts.priority.modeLabel') }}
          </span>
          <ZSegmented
            v-model="mode"
            class="w-full"
            :aria-label="t('accounts.priority.modeAria')"
            :options="modeOptions"
            :disabled="saving"
          />
        </div>
        <div class="grid w-64 gap-1.5">
          <span class="text-[10px] font-bold text-cp-text-quaternary">
            {{ t('accounts.priority.autoOff') }}
          </span>
          <ZSegmented
            v-model="autoOffChoice"
            class="w-full"
            :aria-label="t('accounts.priority.autoOffAria')"
            :options="autoOffOptions"
            :disabled="saving"
          />
        </div>
        <div class="flex items-center gap-2">
          <span v-if="dirty" class="size-1.5 shrink-0 rounded-full bg-cp-warning" aria-hidden="true" />
          <ZIconButton
            v-if="dirty"
            :aria-label="t('accounts.priority.undo')"
            variant="solid"
            :disabled="saving"
            @click="reset"
          >
            <Undo2 class="size-4" />
          </ZIconButton>
          <ZButton type="primary" :loading="saving" :disabled="saving || !dirty" @click="save">
            <template #icon>
              <Save class="size-4" />
            </template>
            {{ t('accounts.priority.save') }}
          </ZButton>
        </div>
      </div>
      <p v-if="statusText && !dirty" role="status" class="m-0 mt-2 text-cp-xs font-emphasis" :class="statusTone">
        {{ statusText }}
      </p>
    </template>
  </section>
</template>
