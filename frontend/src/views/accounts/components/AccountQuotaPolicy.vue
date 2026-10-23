<script setup lang="ts">
import type { Account, QuotaPolicy } from '@/api'
import { ZButton, ZConfirmDialog, ZEmpty, ZIconButton, ZPopover, ZSegmented, ZSkeleton } from '@codex-proxy/ui'

import { CircleAlert, RefreshCw, Save, TriangleAlert, Undo2 } from '@lucide/vue'

import { computed, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { hasAutoReset, policyFromForm, quotaPolicyWindows } from '@/views/settings/components/quota/model'
import QuotaPolicyFields from '@/views/settings/components/quota/QuotaPolicyFields.vue'
import { useAccountQuotaPolicy } from '../composables/useAccountQuotaPolicy'

const props = defineProps<{ account: Account }>()

const { t } = useI18n({ useScope: 'global' })

const {
  view,
  globalPolicy,
  globalLoading,
  globalError,
  mode,
  form,
  loading,
  saving,
  loadError,
  conflict,
  dirty,
  validation,
  source,
  status,
  load,
  save,
  reset,
} = useAccountQuotaPolicy({ accountId: () => props.account.id })

const modeOptions = computed(() => [
  { value: 'inherit' as const, label: t('accounts.quotaPolicy.modes.inherit') },
  { value: 'disabled' as const, label: t('accounts.quotaPolicy.modes.disabled') },
  { value: 'custom' as const, label: t('accounts.quotaPolicy.modes.custom') },
])

const validationText = computed(() => {
  switch (validation.value) {
    case 'threshold': return t('settings.quotaPolicy.validation.threshold')
    case 'attempts': return t('settings.quotaPolicy.validation.attempts')
    case 'cooldown': return t('settings.quotaPolicy.validation.cooldown')
    default: return ''
  }
})

const sourceText = computed(() => {
  switch (source.value) {
    case 'global': return t('accounts.quotaPolicy.source.global')
    case 'disabled': return t('accounts.quotaPolicy.source.disabled')
    case 'account': return t('accounts.quotaPolicy.source.account')
    default: return ''
  }
})

const reasonText = computed(() => {
  switch (status.value?.reason) {
    case 'off': return t('accounts.quotaPolicy.reasons.off')
    case 'ready': return t('accounts.quotaPolicy.reasons.ready')
    case 'threshold_primary': return t('accounts.quotaPolicy.reasons.threshold_primary')
    case 'threshold_secondary': return t('accounts.quotaPolicy.reasons.threshold_secondary')
    case 'quota_unconfirmed': return t('accounts.quotaPolicy.reasons.quota_unconfirmed')
    case 'reset_pending': return t('accounts.quotaPolicy.reasons.reset_pending')
    case 'reset_failed': return t('accounts.quotaPolicy.reasons.reset_failed')
    case 'reset_readback_unconfirmed': return t('accounts.quotaPolicy.reasons.reset_readback_unconfirmed')
    case 'auto_budget_exhausted': return t('accounts.quotaPolicy.reasons.auto_budget_exhausted')
    case 'auto_cooldown': return t('accounts.quotaPolicy.reasons.auto_cooldown')
    default: return t('accounts.quotaPolicy.reasons.unknown')
  }
})

function windowSummaryLabel(window: 'primary' | 'secondary') {
  return window === 'primary' ? t('settings.quotaPolicy.windows.primary') : t('settings.quotaPolicy.windows.secondary')
}

function effectiveSummary(policy: QuotaPolicy | undefined) {
  return policy
    ? quotaPolicyWindows(policy).map(({ window, action, thresholdPercent }) => {
        const label = windowSummaryLabel(window)
        switch (action) {
          case 'off': return t('settings.quotaPolicy.summary.off', { window: label })
          case 'stop': return t('settings.quotaPolicy.summary.stop', { window: label, threshold: thresholdPercent })
          default: return t('settings.quotaPolicy.summary.reset', { window: label, threshold: thresholdPercent })
        }
      }).join(' · ')
    : ''
}

// 草稿继承的预览与耗卡判断只来自当前全局策略，不能用账号已保存模式的 effectivePolicy 冒充
const inheritPreview = computed(() => mode.value === 'inherit' ? globalPolicy.value?.policy : undefined)
const inheritSummary = computed(() => effectiveSummary(inheritPreview.value))
const inheritAutoReset = computed(() => mode.value === 'inherit' && !!inheritPreview.value && hasAutoReset(inheritPreview.value))
const customAutoReset = computed(() => mode.value === 'custom' && hasAutoReset(policyFromForm(form.value)))
const confirmNeeded = computed(() => customAutoReset.value || inheritAutoReset.value)
const inheritBlocked = computed(() => mode.value === 'inherit' && (globalLoading.value || !globalPolicy.value || !!globalError.value))
const confirmPolicy = computed(() => mode.value === 'inherit'
  ? inheritPreview.value
  : mode.value === 'custom' ? policyFromForm(form.value) : undefined)

const statusTone = computed(() => status.value?.paused || status.value?.pendingOperationId
  ? 'bg-cp-warning-container text-cp-warning-on-container'
  : 'bg-cp-fill-tertiary text-cp-text-secondary')
const statusText = computed(() => status.value?.pendingOperationId
  ? t('accounts.quotaPolicy.status.pending')
  : status.value?.paused ? t('accounts.quotaPolicy.status.paused') : t('accounts.quotaPolicy.status.active'))

const confirming = ref(false)
function requestSave() {
  if (!dirty.value || validation.value || saving.value)
    return
  if (confirmNeeded.value) {
    confirming.value = true
    return
  }
  void save()
}
function confirmSave() {
  confirming.value = false
  void save()
}
</script>

<template>
  <section
    class="rounded-lg bg-cp-bg-container p-4 shadow-cp-tertiary lg:col-span-2"
    :aria-label="t('accounts.quotaPolicy.title')"
  >
    <div class="flex flex-wrap items-start gap-3">
      <div class="min-w-0">
        <div class="flex items-center gap-1">
          <h3 class="m-0 text-cp-lg font-heavy text-cp-text">
            {{ t('accounts.quotaPolicy.title') }}
          </h3>
          <ZPopover class="-my-1" trigger="hover-click" placement="top-start">
            <template #reference="{ open }">
              <ZIconButton
                :aria-label="t('accounts.quotaPolicy.help.label')"
                :title="undefined"
                :aria-expanded="open"
                class="size-6! hover:bg-transparent! active:bg-transparent!"
              >
                <CircleAlert class="size-3.5" aria-hidden="true" />
              </ZIconButton>
            </template>
            <div class="grid max-w-72 gap-2 px-3 py-2 text-cp-sm leading-relaxed text-cp-text-secondary">
              <p>{{ t('accounts.quotaPolicy.help.ratio') }}</p>
              <p>{{ t('accounts.quotaPolicy.help.stop') }}</p>
              <p>{{ t('accounts.quotaPolicy.help.autoReset') }}</p>
            </div>
          </ZPopover>
          <ZIconButton
            v-if="view"
            :aria-label="t('accounts.quotaPolicy.refresh')"
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
        <p v-if="view" class="m-0 mt-1 text-cp-xs font-emphasis text-cp-text-secondary">
          {{ sourceText }}
          <span aria-hidden="true">·</span>
          <span class="rounded-cp-sm px-1.5 py-0.5" :class="statusTone">{{ statusText }}</span>
        </p>
      </div>
    </div>

    <div v-if="loading && !view" class="mt-3 grid gap-4 sm:grid-cols-[16rem_16rem]" role="status" aria-busy="true">
      <span class="sr-only">{{ t('common.status.loading') }}</span>
      <div v-for="field in 2" :key="field" class="grid w-64 gap-1.5">
        <ZSkeleton shape="text" class="w-14" />
        <ZSkeleton class="h-cp-control w-full" />
      </div>
    </div>

    <ZEmpty
      v-else-if="loadError"
      :title="t('accounts.quotaPolicy.loadFailed')"
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

    <template v-else-if="view">
      <p v-if="conflict" role="status" class="m-0 mt-2 text-cp-xs font-emphasis text-cp-warning">
        {{ t('accounts.quotaPolicy.conflict') }}
      </p>

      <div class="mt-3 grid gap-1.5">
        <span class="text-[10px] font-bold text-cp-text-quaternary">{{ t('accounts.quotaPolicy.modeLabel') }}</span>
        <ZSegmented
          v-model="mode"
          class="w-64 max-w-full"
          :aria-label="t('accounts.quotaPolicy.modeAria')"
          :options="modeOptions"
          :disabled="saving"
        />
      </div>

      <p class="m-0 mt-2 text-cp-xs leading-relaxed" :class="status?.paused || status?.pendingOperationId ? 'text-cp-warning-text' : 'text-cp-text-secondary'" role="status">
        {{ reasonText }}
      </p>

      <QuotaPolicyFields
        v-if="mode === 'custom'"
        v-model="form"
        class="mt-3"
        :disabled="saving"
      />

      <div v-else class="mt-2 grid gap-1 text-cp-xs leading-relaxed text-cp-text-secondary">
        <template v-if="mode === 'inherit'">
          <p v-if="globalLoading && !globalPolicy" class="m-0">
            {{ t('accounts.quotaPolicy.globalPreviewLoading') }}
          </p>
          <p v-else-if="globalError" class="m-0 text-cp-error-text" role="alert">
            {{ t('accounts.quotaPolicy.globalPreviewFailed') }}
            <button type="button" class="ml-1 underline underline-offset-2" @click="load()">
              {{ t('common.actions.retry') }}
            </button>
          </p>
          <template v-else>
            <p class="m-0">
              {{ t('accounts.quotaPolicy.inheritSummary', { summary: inheritSummary }) }}
            </p>
            <p class="m-0">
              {{ t('accounts.quotaPolicy.inheritFollowsGlobal') }}
            </p>
          </template>
          <p v-if="inheritAutoReset && inheritPreview" class="m-0 text-cp-warning-text">
            {{ t('accounts.quotaPolicy.inheritAutoReset', {
              max: inheritPreview.autoReset.maxAttemptsPer24h,
              cooldown: inheritPreview.autoReset.cooldownSeconds,
            }) }}
          </p>
        </template>
        <p v-else class="m-0">
          {{ t('accounts.quotaPolicy.disabledNote') }}
        </p>
      </div>

      <p v-if="validationText && mode === 'custom'" class="m-0 mt-2 text-cp-sm text-cp-error-text" role="alert">
        {{ validationText }}
      </p>

      <div class="mt-3 flex flex-wrap items-center justify-between gap-2">
        <span class="text-[10px] text-cp-text-tertiary">{{ t('accounts.quotaPolicy.separateSave') }}</span>
        <div class="ml-auto flex items-center gap-2">
          <span v-if="dirty" class="size-1.5 shrink-0 rounded-full bg-cp-warning" aria-hidden="true" />
          <ZIconButton v-if="dirty" :aria-label="t('accounts.quotaPolicy.undo')" variant="solid" :disabled="saving" @click="reset">
            <Undo2 class="size-4" />
          </ZIconButton>
          <ZButton type="primary" :loading="saving" :disabled="saving || !dirty || inheritBlocked || (mode === 'custom' && !!validation)" @click="requestSave">
            <template #icon>
              <Save class="size-4" />
            </template>
            {{ t('accounts.quotaPolicy.save') }}
          </ZButton>
        </div>
      </div>
    </template>

    <ZConfirmDialog
      v-model="confirming"
      :title="t('settings.quotaPolicy.confirm.title')"
      :description="mode === 'inherit'
        ? t('accounts.quotaPolicy.inheritConfirmDescription')
        : t('settings.quotaPolicy.confirm.accountDescription')"
      :confirm-text="t('settings.quotaPolicy.confirm.confirm')"
      :loading="saving"
      @confirm="confirmSave"
    >
      <p class="m-0 text-cp-sm leading-relaxed">
        {{ t('settings.quotaPolicy.confirm.body', {
          max: confirmPolicy?.autoReset.maxAttemptsPer24h ?? 1,
          cooldown: confirmPolicy?.autoReset.cooldownSeconds ?? 86400,
        }) }}
      </p>
      <p v-if="mode === 'inherit'" class="m-0 mt-2 text-cp-sm leading-relaxed">
        {{ t('accounts.quotaPolicy.inheritConfirmFollowsGlobal') }}
      </p>
    </ZConfirmDialog>
  </section>
</template>
