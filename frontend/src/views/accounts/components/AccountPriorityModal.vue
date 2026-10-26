<script setup lang="ts">
import type { Account } from '@/api'

import { ZButton, ZDialog } from '@codex-proxy/ui'
import { useI18n } from 'vue-i18n'
import AccountIdentityCell from './AccountIdentityCell.vue'
import AccountPriorityPolicy from './AccountPriorityPolicy.vue'

defineProps<{
  account: Account | null
}>()

const open = defineModel<boolean>({ required: true })
const { t } = useI18n({ useScope: 'global' })
</script>

<template>
  <ZDialog
    v-model="open"
    :title="t('accounts.priority.title')"
    :description="t('accounts.priority.modalDescription')"
    width="42rem"
  >
    <div v-if="account" class="grid gap-4">
      <div class="flex items-center justify-between gap-4 rounded-cp bg-cp-fill-quaternary px-4 py-3">
        <AccountIdentityCell :account="account" size="md" show-plan />
      </div>
      <AccountPriorityPolicy :account="account" embedded />
    </div>

    <template #footer>
      <ZButton @click="open = false">
        {{ t('common.actions.close') }}
      </ZButton>
    </template>
  </ZDialog>
</template>
