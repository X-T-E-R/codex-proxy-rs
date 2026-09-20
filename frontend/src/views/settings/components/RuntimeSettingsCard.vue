<script setup lang="ts">
import { Gauge, Timer, Zap } from '@lucide/vue'

import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseForm from '@/components/base/BaseForm/index.vue'
import BaseInput from '@/components/base/BaseInput.vue'

defineProps<{
  disabled: boolean
  capacityQueueErrors: { retry: string, timeout: string }
}>()

const maxConcurrentPerAccount = defineModel<string>('maxConcurrentPerAccount', { required: true })
const refreshMarginSeconds = defineModel<string>('refreshMarginSeconds', { required: true })
const refreshConcurrency = defineModel<string>('refreshConcurrency', { required: true })
const requestIntervalMs = defineModel<string>('requestIntervalMs', { required: true })
const capacityQueueRetrySeconds = defineModel<string>('capacityQueueRetrySeconds', { required: true })
const capacityQueueTimeoutSeconds = defineModel<string>('capacityQueueTimeoutSeconds', { required: true })
</script>

<template>
  <BaseCard
    title="运行参数"
    description="请求节奏、账号并发、满载排队和 Token 刷新"
  >
    <BaseForm class="max-w-6xl sm:grid-cols-2">
      <BaseFormItem
        label="单账号默认最大并发"
        description="账号未单独设置时使用的并发上限"
      >
        <BaseInput
          v-model="maxConcurrentPerAccount"
          aria-label="单账号默认最大并发"
          type="number"
          :disabled="disabled"
        >
          <template #prefix>
            <Gauge class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="提前刷新秒数"
        description="Token 过期前多少秒触发刷新"
      >
        <BaseInput
          v-model="refreshMarginSeconds"
          aria-label="提前刷新秒数"
          type="number"
          :disabled="disabled"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="刷新并发数"
        description="同时刷新 Token 的最大请求数，减小可避免限流"
      >
        <BaseInput
          v-model="refreshConcurrency"
          aria-label="刷新并发数"
          type="number"
          :disabled="disabled"
        >
          <template #prefix>
            <Zap class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="请求间隔 ms"
        description="控制同一账号两次调度之间的最小等待时间"
      >
        <BaseInput
          v-model="requestIntervalMs"
          aria-label="请求间隔 ms"
          type="number"
          :disabled="disabled"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="满载重试间隔（秒）"
        description="所有可用账号并发已满时，默认每 3 秒重新选择一次"
        :error="capacityQueueErrors.retry"
        required
      >
        <BaseInput
          v-model="capacityQueueRetrySeconds"
          aria-label="满载重试间隔（秒）"
          type="number"
          min="1"
          step="1"
          :max="4_294_967_295"
          :disabled="disabled"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="满载排队超时（秒）"
        description="默认等待 60 秒；超时后返回原有容量不足错误"
        :error="capacityQueueErrors.timeout"
        required
      >
        <BaseInput
          v-model="capacityQueueTimeoutSeconds"
          aria-label="满载排队超时（秒）"
          type="number"
          min="1"
          step="1"
          :max="4_294_967_295"
          :disabled="disabled"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>
    </BaseForm>
  </BaseCard>
</template>
