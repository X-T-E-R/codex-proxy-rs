<script setup lang="ts">
import { ZButton, ZCard, ZEmpty, ZIconButton, ZPopover, ZSegmented, ZSkeleton, ZSwitch } from '@codex-proxy/ui'

import { CircleAlert, Save, TriangleAlert, Undo2 } from '@lucide/vue'

import { computed, onMounted } from 'vue'
import { useI18n } from 'vue-i18n'
import { useGlobalPriorityPolicy } from '../composables/useGlobalPriorityPolicy'

const { t } = useI18n({ useScope: 'global' })

const { mode, autoOffOnQuotaReset, loading, saving, loadError, conflict, dirty, load, save, reset } = useGlobalPriorityPolicy()

const modeOptions = computed(() => [
  { label: t('common.values.default'), value: 'default' },
  { label: t('common.status.enabled'), value: 'enabled' },
  { label: t('common.status.disabled'), value: 'disabled' },
])
const modeHelp = computed(() => {
  switch (mode.value) {
    case 'enabled': return t('settings.priority.modeHelp.enabled')
    case 'disabled': return t('settings.priority.modeHelp.disabled')
    default: return t('settings.priority.modeHelp.default')
  }
})

onMounted(() => {
  void load()
})
</script>

<template>
  <ZCard :title="t('settings.priority.title')">
    <template #description>
      <span class="inline-flex items-center gap-1">
        {{ t('settings.priority.description') }}
        <ZPopover class="-my-1" trigger="hover-click" placement="top-start">
          <template #reference="{ open }">
            <ZIconButton
              :aria-label="t('settings.priority.help.label')"
              :title="undefined"
              :aria-expanded="open"
              class="size-6! hover:bg-transparent! active:bg-transparent!"
            >
              <CircleAlert class="size-3.5" aria-hidden="true" />
            </ZIconButton>
          </template>
          <div class="grid max-w-72 gap-2 px-3 py-2 text-cp-sm leading-relaxed text-cp-text-secondary">
            <p>{{ t('settings.priority.help.groupCap') }}</p>
            <p>{{ t('settings.priority.help.explicitTier') }}</p>
            <p>{{ t('settings.priority.help.scope') }}</p>
          </div>
        </ZPopover>
      </span>
    </template>

    <div v-if="loading" class="grid max-w-6xl gap-3" role="status" aria-busy="true">
      <span class="sr-only">{{ t('common.status.loading') }}</span>
      <ZSkeleton class="h-cp-control w-72 max-w-full" />
      <ZSkeleton shape="text" class="w-64 max-w-full opacity-70" />
    </div>

    <ZEmpty
      v-else-if="loadError"
      :title="t('settings.priority.loadFailed')"
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

    <div v-else class="grid max-w-6xl gap-3">
      <div class="flex flex-wrap items-start justify-between gap-4">
        <div class="grid min-w-0 gap-3">
          <ZSegmented
            v-model="mode"
            class="w-72 max-w-full"
            :aria-label="t('settings.priority.title')"
            :options="modeOptions"
            :disabled="saving"
          />
          <p class="m-0 text-cp-sm text-cp-text-secondary">
            {{ modeHelp }}
          </p>
          <div class="flex flex-wrap items-center gap-x-3 gap-y-1.5">
            <ZSwitch
              v-model="autoOffOnQuotaReset"
              :label="t('settings.priority.autoOff')"
              :disabled="saving"
            />
            <span class="text-cp-xs text-cp-text-tertiary">{{ t('settings.priority.autoOffHint') }}</span>
          </div>
        </div>
        <div class="flex items-center gap-2">
          <span v-if="dirty" class="size-1.5 shrink-0 rounded-full bg-cp-warning" aria-hidden="true" />
          <ZIconButton
            v-if="dirty"
            :aria-label="t('settings.priority.undo')"
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
            {{ t('settings.priority.save') }}
          </ZButton>
        </div>
      </div>
      <p v-if="conflict" role="status" class="m-0 text-cp-sm font-emphasis text-cp-warning">
        {{ t('settings.priority.conflict') }}
      </p>
    </div>
  </ZCard>
</template>
