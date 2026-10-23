<script setup lang="ts">
import { ZCard, ZForm, ZFormItem, ZIconButton, ZInput, ZPopover, ZSwitch } from '@codex-proxy/ui'

import { CircleAlert, Timer } from '@lucide/vue'

import { useId } from 'vue'
import { useI18n } from 'vue-i18n'

const { t } = useI18n({ useScope: 'global' })

const enabled = defineModel<boolean>('enabled', { required: true })
const seconds = defineModel<string>('seconds', { required: true })
const scopeHintId = useId()
</script>

<template>
  <ZCard :title="t('settings.cyber.title')" :description="t('settings.cyber.description')">
    <ZForm class="max-w-6xl sm:grid-cols-2 sm:items-end">
      <div class="col-span-full flex flex-wrap items-center gap-1">
        <ZSwitch v-model="enabled" :label="t('settings.cyber.enable')" />
        <ZPopover class="-my-1" trigger="hover-click" placement="top-start" :hover-delay="240">
          <template #reference="{ open }">
            <ZIconButton
              :aria-label="t('settings.cyber.help.label')"
              :title="undefined"
              class="size-6! hover:bg-transparent! active:bg-transparent!"
              :aria-expanded="open"
              :aria-describedby="open ? scopeHintId : undefined"
            >
              <CircleAlert class="size-3.5" aria-hidden="true" />
            </ZIconButton>
          </template>
          <div :id="scopeHintId" role="tooltip" class="grid max-w-80 gap-2 px-3 py-2 text-cp-sm leading-relaxed text-cp-text-secondary">
            <p>{{ t('settings.cyber.help.scope') }}</p>
            <p>{{ t('settings.cyber.help.boundary') }}</p>
            <p>{{ t('settings.cyber.help.off') }}</p>
          </div>
        </ZPopover>
      </div>

      <ZFormItem :label="t('settings.cyber.ttl')" :description="t('settings.cyber.ttlHelp')">
        <ZInput
          v-model="seconds"
          :aria-label="t('settings.cyber.ttl')"
          type="number"
          min="1"
          max="4294967295"
          step="1"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
          <template #suffix>
            <span class="text-cp-sm">{{ t('settings.cyber.seconds') }}</span>
          </template>
        </ZInput>
      </ZFormItem>
    </ZForm>
  </ZCard>
</template>
