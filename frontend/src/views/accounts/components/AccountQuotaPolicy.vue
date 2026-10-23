<script setup lang="ts">
import type { Account } from '@/api'
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
  mode,
  form,
  loading,
  saving,
  loadError,
  conflict,
  dirty,
  validation,
  effectivePolicy,
  source,
  status,
  load,
  save,
  reset,
} = useAccountQuotaPolicy({ accountId: () => props.account.id })

const modeOptions = computed(() => (['inherit', 'disabled', 'custom'] as const).map(value => ({
  value,
  label: t(`accounts.quotaPolicy.modes.${value}`),
})))

const validationText = computed(() => validation.value ? t(`settings.quotaPolicy.validation.${validation.value}`) : '')
const sourceText = computed(() => source.value ? t(`accounts.quotaPolicy.source.${source.value}`) : '')
const reasonText = computed(() => t(`accounts.quotaPolicy.reasons.${status.value?.reason ?? 'unknown'}`))

const effectiveSummary = computed(() => effectivePolicy.value
  ? quotaPolicyWindows(effectivePolicy.value).map(({ window, action, thresholdPercent }) => {
      const label = window === 'primary' ? t('settings.quotaPolicy.windows.primary') : t('settings.quotaPolicy.windows.secondary')
      if (action === 'off')
        return t('settings.quotaPolicy.summary.off', { window: label })
      return t(action === 'stop' ? 'settings.quotaPolicy.summary.stop' : 'settings.quotaPolicy.summary.reset', { window: label, threshold: thresholdPercent })
    }).join(' · ')
  : '')

const inheritAutoReset = computed(() => mode.value !== 'disabled' && effectivePolicy.value ? hasAutoReset(effectivePolicy.value) : false)
const confirmNeeded = computed(() => mode.value === 'custom' && hasAutoReset(policyFromForm(form.value)))

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
        <p class="m-0">
          {{ mode === 'inherit'
            ? t('accounts.quotaPolicy.inheritSummary', { summary: effectiveSummary })
            : t('accounts.quotaPolicy.disabledNote') }}
        </p>
        <p v-if="mode === 'inherit' && inheritAutoReset && effectivePolicy" class="m-0 text-cp-warning-text">
          {{ t('accounts.quotaPolicy.inheritAutoReset', {
            max: effectivePolicy.autoReset.maxAttemptsPer24h,
            cooldown: effectivePolicy.autoReset.cooldownSeconds,
          }) }}
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
          <ZButton type="primary" :loading="saving" :disabled="saving || !dirty || (mode === 'custom' && !!validation)" @click="requestSave">
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
      :description="t('settings.quotaPolicy.confirm.accountDescription')"
      :confirm-text="t('settings.quotaPolicy.confirm.confirm')"
      :loading="saving"
      @confirm="confirmSave"
    >
      <p class="m-0 text-cp-sm leading-relaxed">
        {{ t('settings.quotaPolicy.confirm.body', {
          max: policyFromForm(form).autoReset.maxAttemptsPer24h,
          cooldown: policyFromForm(form).autoReset.cooldownSeconds,
        }) }}
      </p>
    </ZConfirmDialog>
  </section>
</template>
