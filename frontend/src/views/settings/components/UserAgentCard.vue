<script setup lang="ts">
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'

defineProps<{ disabled: boolean, error: string }>()
const model = defineModel<string>({ required: true })
const windowsTemplate = '{originator}/{codex_version} (Windows 10.0.26100; x86_64) unknown ({originator}; {desktop_version})'
</script>

<template>
  <BaseCard
    title="OpenAI 上游 User-Agent"
    description="保存后约 5 秒内用于新的 HTTP 请求和 WebSocket 握手；Dashboard 会显示最终值及其来源"
  >
    <BaseFormItem
      label="User-Agent 模板"
      description="支持 {originator}、{codex_version}、{desktop_version}，每次生成请求头时替换为当前值；版本继续自动更新。留空使用启动画像，填写固定版本号则保持原文。这里只改变 User-Agent；环境与设备 metadata 由独立开关控制。修改可能使依赖旧 WebSocket 连接的续接失效"
      :error="error"
    >
      <BaseInput
        v-model="model"
        aria-label="OpenAI 上游 User-Agent"
        placeholder="留空使用自动画像"
        :maxlength="512"
        :disabled="disabled"
      />
      <BaseButton class="mt-3" :disabled="disabled" @click="model = windowsTemplate">
        使用 Windows 模板
      </BaseButton>
    </BaseFormItem>
  </BaseCard>
</template>
