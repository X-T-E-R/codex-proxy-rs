<script setup lang="ts">
import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseForm from '@/components/base/BaseForm/index.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'

defineProps<{
  disabled: boolean
  error: string
}>()

const enabled = defineModel<boolean>('enabled', { required: true })
const seconds = defineModel<string>('seconds', { required: true })
</script>

<template>
  <BaseCard
    title="cyber 会话自动屏蔽"
    description="开启后，被上游网络安全策略（cyber_policy）拦截的会话将在屏蔽时长内被本地拒绝，不再发往上游"
  >
    <p class="mb-4 text-cp-sm text-cp-text-secondary">
      按 Client Key 与会话隔离，不屏蔽整个账号，也不影响同 Key 的其他会话。
      没有会话 ID 时，只匹配被拒绝的完整历史及其精确续接；不根据缓存 Key、公共提示词或普通文本判断。
    </p>
    <BaseForm class="max-w-6xl sm:grid-cols-2">
      <BaseFormItem
        label="启用 cyber 会话自动屏蔽"
        description="默认关闭；保存后新请求采用新设置，关闭后忽略已有屏蔽记录"
      >
        <BaseSwitch v-model="enabled" :disabled="disabled" label="启用 cyber 会话自动屏蔽" />
      </BaseFormItem>
      <BaseFormItem
        label="屏蔽时长（秒）"
        description="默认 3600 秒（1 小时）；已有记录保持首次屏蔽的到期时间，重试不会延长"
        :error="error"
        required
      >
        <BaseInput
          v-model="seconds"
          aria-label="屏蔽时长（秒）"
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
