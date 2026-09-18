<script setup lang="ts">
import type { AccountRow } from '../constants'
import { ref, watch } from 'vue'

import BaseButton from '@/components/base/BaseButton.vue'
import BaseModal from '@/components/base/BaseModal/index.vue'
import AccountModelTurnStateSection from './AccountModelTurnStateSection.vue'

const props = defineProps<{ account: AccountRow | null }>()
const open = defineModel<boolean>({ required: true })
const busy = ref(false)

watch([open, () => props.account?.id], () => {
  busy.value = false
})
</script>

<template>
  <BaseModal
    v-model="open"
    title="Turn State"
    description="按实际上游模型管理当前值、待切换值与自动捕获。旧账号覆盖只作为待导入值，不再参与请求回退。"
    size="xl"
    :dismissible="!busy"
  >
    <div class="grid gap-5 text-cp">
      <p v-if="account" class="m-0 break-all text-cp-text-secondary">
        账号：{{ account.email || account.accountId || account.id }}
      </p>
      <AccountModelTurnStateSection
        v-if="account"
        :account-id="account.id"
        :open="open"
        @busy-change="busy = $event"
      />
    </div>
    <template #footer>
      <BaseButton variant="secondary" :disabled="busy" @click="open = false">
        关闭
      </BaseButton>
    </template>
  </BaseModal>
</template>
