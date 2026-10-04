<script setup lang="ts">
import type { AccountQuotaPolicy } from '@/api'
import { shallowRef, watch } from 'vue'
import { getAccountQuotaPolicy } from '@/api'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseSkeleton from '@/components/base/BaseSkeleton.vue'
import QuotaPolicyStatus from '@/components/quota-policy/QuotaPolicyStatus.vue'
import { useRequestState } from '@/composables/useRequestState'

const props = defineProps<{ accountId: string, refreshedQuota: object }>()
const request = useRequestState()
const { loading, error } = request
const value = shallowRef<AccountQuotaPolicy | null>(null)

async function load() {
  const id = request.start()
  try {
    const result = await getAccountQuotaPolicy({ accountId: props.accountId }, { silent: true, signal: request.signal })
    if (request.isCurrent(id))
      value.value = result
  }
  catch (cause) {
    request.fail(id, cause)
  }
  finally {
    request.finish(id)
  }
}

watch(() => [props.accountId, props.refreshedQuota], () => {
  value.value = null
  void load()
}, { immediate: true })
</script>

<template>
  <section class="grid gap-2" aria-label="账号额度策略" :aria-busy="loading">
    <div class="flex flex-wrap items-center justify-between gap-2">
      <h4 class="m-0 text-cp-sm font-heavy text-cp-text">
        额度策略
      </h4>
      <BaseButton size="sm" variant="ghost" :disabled="loading" @click="load">
        刷新策略
      </BaseButton>
    </div>
    <BaseSkeleton v-if="loading && !value" class="h-16 w-full" />
    <p v-if="error" class="m-0 text-cp-xs leading-relaxed text-cp-error-text" role="alert">
      策略状态读取失败，请刷新策略重试
    </p>
    <QuotaPolicyStatus v-else-if="value" :value="value" />
  </section>
</template>
