<script setup lang="ts">
import type { AccountQuotaPolicy, AccountQuotaPolicyMode, GlobalQuotaPolicy } from '@/api'
import { computed, ref, shallowRef, watch } from 'vue'
import { getAccountQuotaPolicy, getGlobalQuotaPolicy, updateAccountQuotaPolicy, updateGlobalQuotaPolicy } from '@/api'
import { ApiError } from '@/api/request'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseConfirmModal from '@/components/base/BaseConfirmModal.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseSkeleton from '@/components/base/BaseSkeleton.vue'
import { toast } from '@/components/base/BaseToast'
import { useRequestState } from '@/composables/useRequestState'
import { errorMessage } from '@/utils/async'
import { hasAutoReset, policyFromForm, quotaModeOptions, quotaPolicyError, quotaPolicyForm, quotaPolicySummary } from './model'
import QuotaPolicyFields from './QuotaPolicyFields.vue'
import QuotaPolicyStatus from './QuotaPolicyStatus.vue'

const props = defineProps<{
  accountId?: string
  disabled?: boolean
}>()
const emit = defineEmits<{
  saving: [value: boolean]
  saved: []
}>()
const request = useRequestState()
const { loading } = request
const global = shallowRef<GlobalQuotaPolicy | null>(null)
const account = shallowRef<AccountQuotaPolicy | null>(null)
const form = ref(quotaPolicyForm())
const mode = shallowRef<AccountQuotaPolicyMode>('inherit')
const baseline = shallowRef('')
const saving = shallowRef(false)
const error = shallowRef('')
const needsReload = shallowRef(false)
const confirming = shallowRef(false)
const signature = computed(() => JSON.stringify({ form: form.value, mode: mode.value }))
const changed = computed(() => baseline.value !== '' && signature.value !== baseline.value)
const editable = computed(() => !props.accountId || mode.value === 'custom')
const validation = computed(() => editable.value ? quotaPolicyError(form.value) : '')
const busy = computed(() => loading.value || saving.value || !!props.disabled)
const ready = computed(() => !!global.value && (!props.accountId || !!account.value))
const saveDisabled = computed(() => busy.value || !ready.value || needsReload.value || !changed.value || !!validation.value)
const effectiveDraft = computed(() => mode.value === 'inherit' && props.accountId ? global.value?.policy : policyFromForm(form.value))

async function load() {
  const id = request.start()
  error.value = ''
  try {
    const options = { silent: true, signal: request.signal }
    const [globalValue, accountValue] = await Promise.all([
      getGlobalQuotaPolicy(options),
      props.accountId ? getAccountQuotaPolicy({ accountId: props.accountId }, options) : Promise.resolve(null),
    ])
    if (!request.isCurrent(id))
      return false
    global.value = globalValue
    account.value = accountValue
    mode.value = accountValue?.mode ?? 'inherit'
    form.value = quotaPolicyForm(accountValue?.policy ?? accountValue?.effectivePolicy ?? globalValue.policy)
    baseline.value = signature.value
    needsReload.value = false
    return true
  }
  catch (cause) {
    if (request.isCurrent(id)) {
      error.value = `额度策略读取失败：${errorMessage(cause)}，请重新读取`
      needsReload.value = true
    }
    return false
  }
  finally {
    request.finish(id)
  }
}

function requestSave() {
  if (saveDisabled.value)
    return
  if ((!props.accountId || mode.value !== 'disabled') && effectiveDraft.value && hasAutoReset(effectiveDraft.value)) {
    confirming.value = true
    return
  }
  void save()
}

async function save() {
  if (saveDisabled.value || !global.value)
    return
  saving.value = true
  emit('saving', true)
  error.value = ''
  try {
    if (props.accountId && account.value) {
      await updateAccountQuotaPolicy({
        accountId: props.accountId,
        expectedRevision: account.value.revision,
        mode: mode.value,
        policy: mode.value === 'custom' ? policyFromForm(form.value) : null,
      }, { silent: true })
    }
    else {
      await updateGlobalQuotaPolicy({
        expectedRevision: global.value.revision,
        policy: policyFromForm(form.value),
      }, { silent: true })
    }
    // 回读期间仍锁定编辑与弹窗关闭，避免旧修订被再次保存。
    const confirmed = await load()
    if (confirmed) {
      emit('saved')
      toast.success('额度策略已保存并回读')
    }
    else if (error.value) {
      error.value = '额度策略已保存，但回读失败，请重新读取确认生效状态'
    }
  }
  catch (cause) {
    needsReload.value = true
    error.value = cause instanceof ApiError && cause.status === 409
      ? '额度策略已被其他操作修改，请重新读取后再编辑'
      : `额度策略保存未确认：${errorMessage(cause)}，请先重新读取，不要重复保存`
  }
  finally {
    confirming.value = false
    saving.value = false
    emit('saving', false)
  }
}

watch(() => props.accountId, () => {
  request.invalidate()
  global.value = null
  account.value = null
  baseline.value = ''
  confirming.value = false
  void load()
}, { immediate: true })
</script>

<template>
  <section class="grid min-w-0 gap-4" aria-label="额度策略编辑" :aria-busy="busy">
    <div v-if="loading && !ready" class="grid gap-3" role="status" aria-label="正在读取额度策略">
      <BaseSkeleton class="h-5 w-40" />
      <BaseSkeleton class="h-16 w-full" />
      <BaseSkeleton class="h-16 w-full" />
    </div>
    <div v-if="error" class="rounded-cp bg-cp-error-container p-3 text-cp-sm leading-relaxed text-cp-error-on-container" role="alert">
      {{ error }}
    </div>
    <template v-if="ready">
      <QuotaPolicyStatus v-if="account" :value="account" />
      <BaseFormItem v-if="accountId" label="本账号额度策略">
        <BaseSelect v-model="mode" class="w-full" :options="quotaModeOptions" :disabled="busy || needsReload" aria-label="本账号额度策略" />
      </BaseFormItem>
      <QuotaPolicyFields v-if="editable" v-model="form" :disabled="busy || needsReload" />
      <div v-else class="grid gap-2 text-cp-sm leading-relaxed text-cp-text-secondary">
        <template v-if="mode === 'inherit' && global">
          <p class="m-0">
            继承全局默认：{{ quotaPolicySummary(global.policy) }}
          </p>
          <p v-if="hasAutoReset(global.policy)" class="m-0 text-cp-warning-text">
            继承后允许不可撤销地消耗重置卡，每账号滚动 24 小时最多 {{ global.policy.autoReset.maxAttemptsPer24h }} 次，最小间隔 {{ global.policy.autoReset.cooldownSeconds }} 秒，未决结果确认前不耗下一卡
          </p>
        </template>
        <p v-else class="m-0">
          本账号不执行额度策略，不影响账号启用状态或其他调度限制
        </p>
      </div>
      <p v-if="validation" class="m-0 text-cp-sm text-cp-error-text" role="alert">
        {{ validation }}
      </p>
    </template>
    <div class="flex flex-wrap items-center justify-between gap-3">
      <span class="text-cp-xs text-cp-text-secondary">{{ accountId ? '额度策略单独保存，不随账号其他字段保存' : '对每个继承账号分别判断，不累计账号池比例' }}</span>
      <div class="ml-auto flex flex-wrap items-center gap-2">
        <BaseButton variant="secondary" :disabled="busy" @click="load">
          {{ changed ? '放弃更改并重读' : '重新读取策略' }}
        </BaseButton>
        <BaseButton variant="primary" :loading="saving" :disabled="saveDisabled" @click="requestSave">
          {{ accountId ? '保存账号额度策略' : '保存全局额度策略' }}
        </BaseButton>
      </div>
    </div>
  </section>
  <BaseConfirmModal
    v-model="confirming"
    title="允许自动消耗重置卡？"
    :description="accountId ? '保存后，本账号达到窗口阈值时可自动消耗主动重置卡' : '保存后，继承全局策略的 Codex OAuth 账号达到阈值时可自动消耗主动重置卡'"
    confirm-text="保存并允许自动重置"
    :loading="saving"
    :confirm-disabled="saveDisabled"
    @confirm="save"
  >
    <p class="m-0 text-cp-sm leading-relaxed">
      消耗不可撤销，额度回读确认前不会显示恢复完成<br>
      每账号滚动 24 小时最多 {{ effectiveDraft?.autoReset.maxAttemptsPer24h }} 次，最小间隔 {{ effectiveDraft?.autoReset.cooldownSeconds }} 秒
    </p>
  </BaseConfirmModal>
</template>
