<script setup lang="ts">
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseForm from '@/components/base/BaseForm/index.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'

defineProps<{
  disabled: boolean
  errors: { timezone: string, country: string }
}>()

const enabled = defineModel<boolean>('enabled', { required: true })
const timezone = defineModel<string>('timezone', { required: true })
const country = defineModel<string>('country', { required: true })

function applyPacificPreset() {
  timezone.value = 'America/Los_Angeles'
  country.value = 'US'
}
</script>

<template>
  <BaseCard
    title="OpenAI 请求正文地域覆盖"
    description="覆盖 Codex 环境上下文的时区、日期，以及搜索请求中的位置"
  >
    <BaseForm class="max-w-6xl sm:grid-cols-2">
      <BaseFormItem
        label="启用正文覆盖"
        description="默认开启；关闭后不再覆盖这些字段"
        class="sm:col-span-2"
      >
        <BaseSwitch
          v-model="enabled"
          label="启用 OpenAI 请求正文地域覆盖"
          :disabled="disabled"
        />
      </BaseFormItem>
      <BaseFormItem
        label="环境与搜索时区"
        description="填写 IANA 时区；太平洋预设自动跟随夏令时，并使用该时区的当前日期"
        :error="errors.timezone"
        required
      >
        <BaseInput
          v-model="timezone"
          aria-label="环境与搜索时区"
          placeholder="America/Los_Angeles"
          :maxlength="128"
          :disabled="disabled || !enabled"
          spellcheck="false"
        />
      </BaseFormItem>
      <BaseFormItem
        label="搜索国家代码"
        description="填写两位字母国家代码，例如 US（美国）、CN（中国）"
        :error="errors.country"
        required
      >
        <BaseInput
          v-model="country"
          aria-label="搜索国家代码"
          placeholder="US"
          :maxlength="2"
          :disabled="disabled || !enabled"
          spellcheck="false"
        />
      </BaseFormItem>
    </BaseForm>
    <BaseButton
      class="mt-3"
      :disabled="disabled || !enabled"
      @click="applyPacificPreset"
    >
      使用美国 / 太平洋预设
    </BaseButton>
    <p class="mt-4 mb-0 text-cp-sm text-cp-text-secondary">
      只覆盖最近的独立环境块，并为已有搜索请求或工具设置位置；不添加搜索工具。
      不改变出口 IP、操作系统设置或数据驻留策略。
    </p>
  </BaseCard>
</template>
