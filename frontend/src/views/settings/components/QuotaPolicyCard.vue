<script setup lang="ts">
import { ZButton, ZCard, ZConfirmDialog, ZEmpty, ZIconButton, ZSkeleton } from '@codex-proxy/ui'

import { RefreshCw, Save, TriangleAlert, Undo2 } from '@lucide/vue'

import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useGlobalQuotaPolicy } from '../composables/useGlobalQuotaPolicy'
import { hasAutoReset } from './quota/model'
import QuotaPolicyFields from './quota/QuotaPolicyFields.vue'

const { t } = useI18n({ useScope: 'global' })

const {
  policy,
  form,
  loading,
  saving,
  loadError,
  conflict,
  dirty,
  validation,
  draft,
  load,
  save,
  reset,
} = useGlobalQuotaPolicy()

const confirming = ref(false)
const validationText = computed(() => {
  switch (validation.value) {
    case 'threshold': return t('settings.quotaPolicy.validation.threshold')
    case 'attempts': return t('settings.quotaPolicy.validation.attempts')
    case 'cooldown': return t('settings.quotaPolicy.validation.cooldown')
    default: return ''
  }
})

onMounted(() => {
  void load()
})

// 自动重置会消耗重置卡，保存前必须由管理员确认这一后果
function requestSave() {
  if (!dirty.value || validation.value || saving.value)
    return
  if (hasAutoReset(draft.value)) {
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
  <ZCard :title="t('settings.quotaPolicy.title')" :description="t('settings.quotaPolicy.description')">
    <div v-if="loading && !policy" class="grid gap-3" role="status" aria-busy="true">
      <span class="sr-only">{{ t('common.status.loading') }}</span>
      <ZSkeleton class="h-cp-control w-full" />
      <ZSkeleton class="h-cp-control w-full" />
    </div>

    <ZEmpty
      v-else-if="loadError"
      :title="t('settings.quotaPolicy.loadFailed')"
      :description="loadError"
      :icon="TriangleAlert"
      surface="none"
      role="alert"
    >
      <template #action>
        <ZButton :loading="loading" @click="load()">
          {{ t('common.actions.retry') }}
        </ZButton>
      </template>
    </ZEmpty>

    <template v-else>
      <p v-if="conflict" role="status" class="m-0 mb-3 text-cp-xs font-emphasis text-cp-warning">
        {{ t('settings.quotaPolicy.conflict') }}
      </p>
      <QuotaPolicyFields v-model="form" :disabled="saving" />
      <p v-if="validationText" class="m-0 mt-3 text-cp-sm text-cp-error-text" role="alert">
        {{ validationText }}
      </p>
      <div class="mt-4 flex flex-wrap items-center justify-between gap-3">
        <span class="text-cp-xs text-cp-text-secondary">{{ t('settings.quotaPolicy.reloadHint') }}</span>
        <div class="ml-auto flex items-center gap-2">
          <span v-if="dirty" class="size-1.5 shrink-0 rounded-full bg-cp-warning" aria-hidden="true" />
          <ZIconButton v-if="dirty" :aria-label="t('settings.quotaPolicy.undo')" variant="solid" :disabled="saving || loading" @click="reset">
            <Undo2 class="size-4" />
          </ZIconButton>
          <ZButton :loading="loading" :disabled="saving" @click="load(conflict)">
            <template #icon>
              <RefreshCw class="size-4" />
            </template>
            {{ t('settings.quotaPolicy.refresh') }}
          </ZButton>
          <ZButton type="primary" :loading="saving" :disabled="saving || loading || !dirty || !!validation" @click="requestSave">
            <template #icon>
              <Save class="size-4" />
            </template>
            {{ t('settings.quotaPolicy.save') }}
          </ZButton>
        </div>
      </div>
    </template>

    <ZConfirmDialog
      v-model="confirming"
      :title="t('settings.quotaPolicy.confirm.title')"
      :description="t('settings.quotaPolicy.confirm.description')"
      :confirm-text="t('settings.quotaPolicy.confirm.confirm')"
      :loading="saving"
      :confirm-disabled="!!validation || !dirty"
      @confirm="confirmSave"
    >
      <p class="m-0 text-cp-sm leading-relaxed">
        {{ t('settings.quotaPolicy.confirm.body', { max: draft.autoReset.maxAttemptsPer24h, cooldown: draft.autoReset.cooldownSeconds }) }}
      </p>
    </ZConfirmDialog>
  </ZCard>
</template>
