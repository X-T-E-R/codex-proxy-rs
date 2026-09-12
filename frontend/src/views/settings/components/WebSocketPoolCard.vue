<script setup lang="ts">
import { Gauge, Timer, Zap } from '@lucide/vue'

import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseForm from '@/components/base/BaseForm/index.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'

defineProps<{
  disabled: boolean
  errors: {
    maxAge: string
    maxConnecting: string
    streamIdleTimeout: string
    fastPathBudget: string
  }
}>()

const wsPoolEnabled = defineModel<boolean>('wsPoolEnabled', { required: true })
const wsPoolMaxAgeMs = defineModel<string>('wsPoolMaxAgeMs', { required: true })
const wsPoolMaxConnecting = defineModel<string>('wsPoolMaxConnecting', { required: true })
const wsPoolStreamIdleTimeoutMs = defineModel<string>('wsPoolStreamIdleTimeoutMs', { required: true })
const wsPoolFastPathBudgetMs = defineModel<string>('wsPoolFastPathBudgetMs', { required: true })
</script>

<template>
  <BaseCard
    title="WebSocket 连接池"
    description="OpenAI 上游长连接复用，保存后约 5 秒内生效，无需重启"
  >
    <BaseForm class="max-w-6xl sm:grid-cols-2">
      <BaseFormItem
        label="启用连接池"
        description="关闭后自动传输请求改用 HTTP SSE；已开始的生成继续完成，需要原连接的续接可能失败"
      >
        <BaseSwitch
          v-model="wsPoolEnabled"
          :disabled="disabled"
          label="启用 WebSocket 连接池"
        />
      </BaseFormItem>

      <BaseFormItem
        label="单连接最大存活 ms"
        description="连接达到此年龄后不再复用；默认 3300000 ms（55 分钟）"
        :error="errors.maxAge"
        required
      >
        <BaseInput
          v-model="wsPoolMaxAgeMs"
          aria-label="单连接最大存活 ms"
          type="number"
          min="1"
          step="1"
          :max="Number.MAX_SAFE_INTEGER"
          :disabled="disabled"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="最大并发建连"
        description="所有账号合计的并发建连上限，非请求并发数；默认 8"
        :error="errors.maxConnecting"
        required
      >
        <BaseInput
          v-model="wsPoolMaxConnecting"
          aria-label="最大并发建连"
          type="number"
          min="1"
          step="1"
          :max="4_294_967_295"
          :disabled="disabled"
        >
          <template #prefix>
            <Gauge class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="流空闲超时 ms"
        description="新请求等待下一条上游业务消息的最长时间；默认 300000 ms（5 分钟）"
        :error="errors.streamIdleTimeout"
        required
      >
        <BaseInput
          v-model="wsPoolStreamIdleTimeoutMs"
          aria-label="流空闲超时 ms"
          type="number"
          min="1"
          step="1"
          :max="Number.MAX_SAFE_INTEGER"
          :disabled="disabled"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="快路径预算 ms"
        description="自动传输请求等待连接就绪的预算，超时后回退 HTTP SSE；默认 800 ms"
        :error="errors.fastPathBudget"
        required
      >
        <BaseInput
          v-model="wsPoolFastPathBudgetMs"
          aria-label="快路径预算 ms"
          type="number"
          min="1"
          step="1"
          :max="Number.MAX_SAFE_INTEGER"
          :disabled="disabled"
        >
          <template #prefix>
            <Zap class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>
    </BaseForm>
  </BaseCard>
</template>
