<script setup lang="ts">
import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseForm from '@/components/base/BaseForm/index.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'

defineProps<{
  disabled: boolean
  errors: { threshold: string, seconds: string }
}>()

const enabled = defineModel<boolean>('enabled', { required: true })
const threshold = defineModel<string>('threshold', { required: true })
const seconds = defineModel<string>('seconds', { required: true })
</script>

<template>
  <BaseCard
    title="上游过载冷号"
    description="同一账号连续返回指定过载错误时暂停接收新请求，包括指定账号和诊断请求；已开始的请求继续完成"
  >
    <p class="mb-4 text-cp-sm text-cp-text-secondary">
      上游错误文本包含任一关键词即计数（包括 HTTP 503）：
      Our servers are currently overloaded. Please try again later.
      或 Selected model is at capacity.
      成功响应或其他错误会清零连续次数。次数按账号在当前进程内累计，重启后重新计数。
    </p>
    <BaseForm class="max-w-6xl sm:grid-cols-2">
      <BaseFormItem
        label="启用过载冷号"
        description="默认关闭，保存后新请求采用新设置；关闭后停止触发，已有冷号仍按原到期时间解除"
      >
        <BaseSwitch v-model="enabled" :disabled="disabled" label="启用过载冷号" />
      </BaseFormItem>
      <BaseFormItem
        label="连续过载次数"
        description="默认 2 次；不同账号分别计数"
        :error="errors.threshold"
        required
      >
        <BaseInput
          v-model="threshold"
          aria-label="连续过载次数"
          type="number"
          min="1"
          step="1"
          :max="4_294_967_295"
          :disabled="disabled"
        />
      </BaseFormItem>
      <BaseFormItem
        label="冷号时长（秒）"
        description="默认 120 秒（2 分钟）；到期自动恢复，在途请求成功也不会提前解冻"
        :error="errors.seconds"
        required
      >
        <BaseInput
          v-model="seconds"
          aria-label="冷号时长（秒）"
          type="number"
          min="1"
          step="1"
          :max="4_294_967_295"
          :disabled="disabled"
        />
      </BaseFormItem>
    </BaseForm>
  </BaseCard>
</template>
