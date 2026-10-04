<script setup lang="ts">
import type { QuotaPolicyForm } from './model'
import { computed } from 'vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import { quotaActionOptions } from './model'

defineProps<{ disabled?: boolean }>()
const form = defineModel<QuotaPolicyForm>({ required: true })
const autoReset = computed(() => form.value.primaryAction === 'reset_then_stop' || form.value.secondaryAction === 'reset_then_stop')
const windows = [
  { key: 'primary', label: '主窗口', detail: 'Primary，短期额度' },
  { key: 'secondary', label: '次窗口', detail: 'Secondary，每周额度' },
] as const
</script>

<template>
  <div class="grid gap-4">
    <div v-for="window in windows" :key="window.key" class="grid items-start gap-3 md:grid-cols-[minmax(8rem,1fr)_minmax(12rem,2fr)_minmax(8rem,1fr)]">
      <div class="pt-1">
        <h4 class="m-0 text-cp-sm font-heavy text-cp-text">
          {{ window.label }}
        </h4>
        <p class="mt-1 mb-0 text-cp-xs text-cp-text-secondary">
          {{ window.detail }}
        </p>
      </div>
      <BaseFormItem :label="`${window.label}动作`">
        <BaseSelect v-model="form[`${window.key}Action`]" class="w-full" :options="quotaActionOptions" :disabled="disabled" :aria-label="`${window.label}动作`" />
      </BaseFormItem>
      <BaseFormItem :label="`${window.label}已用比例 ≥`">
        <BaseInput v-model="form[`${window.key}Threshold`]" type="number" min="1" max="100" step="1" :disabled="disabled || form[`${window.key}Action`] === 'off'" :aria-label="`${window.label}已用比例阈值`">
          <template #suffix>
            %
          </template>
        </BaseInput>
      </BaseFormItem>
    </div>

    <div v-if="autoReset" class="grid gap-3 rounded-cp bg-cp-warning-container p-4 text-cp-warning-on-container">
      <p class="m-0 text-cp-sm font-heavy">
        自动重置会消耗主动重置卡，消耗后不可撤销
      </p>
      <div class="grid gap-3 md:grid-cols-2">
        <BaseFormItem label="每账号滚动 24 小时自动发送上限" description="默认 1 次，允许 1 到 10 次">
          <BaseInput v-model="form.maxAttemptsPer24h" type="number" min="1" max="10" step="1" :disabled="disabled" aria-label="每账号滚动24小时自动发送上限">
            <template #suffix>
              次
            </template>
          </BaseInput>
        </BaseFormItem>
        <BaseFormItem label="自动发送最小间隔" description="默认 86400 秒（24 小时），最少 3600 秒（1 小时）">
          <BaseInput v-model="form.cooldownSeconds" type="number" min="3600" max="86400" step="1" :disabled="disabled" aria-label="自动发送最小间隔秒数">
            <template #suffix>
              秒
            </template>
          </BaseInput>
        </BaseFormItem>
      </div>
      <p class="m-0 text-cp-xs leading-relaxed">
        结果待确认时只确认原操作，不消耗下一张卡<br>
        更换凭据、修改策略或窗口重置都不会清空自动发送预算
      </p>
    </div>
    <p class="m-0 text-cp-xs leading-relaxed text-cp-text-secondary">
      停止仅暂停新分配，不禁用账号，不中断正在进行的请求<br>
      仅按 Codex 窗口已用比例判断，额外点数和容量类 429 不触发自动重置
    </p>
  </div>
</template>
