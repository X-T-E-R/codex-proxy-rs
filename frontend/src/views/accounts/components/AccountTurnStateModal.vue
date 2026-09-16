<script setup lang="ts">
import type { AccountRow } from '../constants'
import type { AccountTurnStateResponse } from '@/api'
import { Copy, Eye, EyeOff, RefreshCw } from '@lucide/vue'
import { computed, ref, watch } from 'vue'

import { getAccountTurnState, updateAccountTurnState, useObservedAccountTurnState } from '@/api'
import { ApiError } from '@/api/request'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseModal from '@/components/base/BaseModal/index.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'
import { toast } from '@/components/base/BaseToast'
import { useCopyText } from '@/composables/useCopyText'
import { errorMessage } from '@/utils/async'
import { formatDateTime } from '@/utils/date'

const props = defineProps<{ account: AccountRow | null }>()
const open = defineModel<boolean>({ required: true })
const copyText = useCopyText()

const state = ref<AccountTurnStateResponse | null>(null)
const loading = ref(false)
const saving = ref(false)
const loadError = ref('')
const actionError = ref('')
const draftValue = ref('')
const draftEnabled = ref(false)
const showObserved = ref(false)
const showOverride = ref(false)
let generation = 0

const changed = computed(() => state.value && (
  draftEnabled.value !== state.value.override.enabled
  || draftValue.value !== (state.value.override.value ?? '')
))
const validationError = computed(() => {
  if (draftEnabled.value && !draftValue.value)
    return '启用覆盖时需填写非空值'
  if (!draftValue.value || (!draftEnabled.value && draftValue.value === (state.value?.override.value ?? '')))
    return ''
  if (new TextEncoder().encode(draftValue.value).length > 16_384)
    return '覆盖值最多 16384 字节（UTF-8）'
  if (/[^\x20-\x7E]/.test(draftValue.value))
    return '覆盖值仅支持可打印 ASCII 字符，不含换行或控制字符'
  return ''
})

function reset() {
  generation++
  state.value = null
  loadError.value = ''
  actionError.value = ''
  draftValue.value = ''
  draftEnabled.value = false
  showObserved.value = false
  showOverride.value = false
  loading.value = false
  saving.value = false
}

function applyState(result: AccountTurnStateResponse) {
  state.value = result
  draftValue.value = result.override.value ?? ''
  draftEnabled.value = result.override.enabled
  showObserved.value = false
  showOverride.value = false
}

async function load() {
  const accountId = props.account?.id
  if (!open.value || !accountId)
    return
  const requestGeneration = ++generation
  loading.value = true
  loadError.value = ''
  actionError.value = ''
  try {
    const result = await getAccountTurnState({ accountId })
    if (requestGeneration === generation)
      applyState(result)
  }
  catch (error) {
    if (requestGeneration === generation)
      loadError.value = errorMessage(error, 'Turn State 读取失败')
  }
  finally {
    if (requestGeneration === generation)
      loading.value = false
  }
}

watch([open, () => props.account?.id], () => {
  reset()
  if (open.value)
    void load()
}, { immediate: true })

async function mutate(action: () => Promise<AccountTurnStateResponse>, successText: string) {
  const requestGeneration = generation
  saving.value = true
  actionError.value = ''
  try {
    const result = await action()
    if (requestGeneration !== generation)
      return
    applyState(result)
    toast.success(successText)
  }
  catch (error) {
    if (requestGeneration !== generation)
      return
    if (error instanceof ApiError && error.status === 409) {
      const accountId = props.account?.id
      await load()
      if (!open.value || props.account?.id !== accountId)
        return
      saving.value = false
      actionError.value = loadError.value
        ? `配置已变化，刷新失败：${loadError.value}`
        : '配置已变化，已载入最新状态；请重新确认后保存。'
    }
    else {
      actionError.value = errorMessage(error, '操作失败')
    }
  }
  finally {
    if (requestGeneration === generation)
      saving.value = false
  }
}

function save() {
  if (!state.value || !props.account || validationError.value || !changed.value || saving.value)
    return
  const current = state.value
  const value = draftValue.value !== (current.override.value ?? '')
    ? (draftValue.value || null)
    : undefined
  void mutate(() => updateAccountTurnState({
    accountId: current.accountId,
    enabled: draftEnabled.value,
    value,
    expectedRevision: current.configRevision,
  }), 'Turn State 覆盖已保存')
}

function clear() {
  if (!state.value || saving.value)
    return
  const current = state.value
  void mutate(() => updateAccountTurnState({
    accountId: current.accountId,
    enabled: false,
    value: null,
    expectedRevision: current.configRevision,
  }), 'Turn State 覆盖已清空')
}

function adoptObserved() {
  if (!state.value?.observed || saving.value)
    return
  const current = state.value
  const observationId = current.observed!.id
  void mutate(() => useObservedAccountTurnState({
    accountId: current.accountId,
    observationId,
    enabled: true,
    expectedRevision: current.configRevision,
  }), '已采用最近观测并启用实验覆盖')
}
</script>

<template>
  <BaseModal
    v-model="open"
    title="Turn State 实验"
    description="不透明的同回合粘性值；手动覆盖仅供实验，不控制账号调度，也不代表模型档位。"
    size="lg"
    :dismissible="!saving"
  >
    <div class="grid gap-5 text-cp">
      <p v-if="account" class="m-0 break-all text-cp-text-secondary">
        账号：{{ account.email || account.accountId || account.id }}
      </p>
      <p v-if="loading" role="status" class="m-0 text-cp-text-secondary">
        正在读取 Turn State…
      </p>
      <div v-else-if="loadError" role="alert" class="grid gap-3 rounded-cp bg-cp-error-container p-4 text-cp-error-on-container">
        <span>{{ loadError }}</span>
        <BaseButton size="sm" class="justify-self-start" @click="load">
          重试
        </BaseButton>
      </div>
      <template v-else-if="state">
        <section class="grid min-w-0 gap-3 rounded-cp bg-cp-fill-quaternary p-4" aria-label="最近上游观测">
          <div class="flex flex-wrap items-center justify-between gap-2">
            <h3 class="m-0 text-cp font-bold text-cp-text">
              最近上游观测
            </h3>
            <BaseButton size="sm" variant="ghost" :disabled="saving || !!changed" title="保存或关闭草稿后刷新" @click="load">
              <template #icon>
                <RefreshCw :size="14" />
              </template>
              刷新
            </BaseButton>
          </div>
          <template v-if="state.observed">
            <div class="flex min-w-0 flex-wrap items-center gap-2">
              <BaseInput :model-value="state.observed.value" :type="showObserved ? 'text' : 'password'" readonly aria-label="最近观测值" class="min-w-40 flex-1" />
              <BaseButton size="sm" :aria-label="showObserved ? '隐藏观测值' : '显示观测值'" @click="showObserved = !showObserved">
                <template #icon>
                  <EyeOff v-if="showObserved" :size="14" /><Eye v-else :size="14" />
                </template>
                {{ showObserved ? '隐藏' : '显示' }}
              </BaseButton>
              <BaseButton size="sm" @click="copyText(state.observed!.value, { successText: '观测值已复制' })">
                <template #icon>
                  <Copy :size="14" />
                </template>
                复制
              </BaseButton>
            </div>
            <dl class="m-0 grid min-w-0 gap-x-4 gap-y-2 text-xs sm:grid-cols-2">
              <div>
                <dt class="text-cp-text-quaternary">
                  长度
                </dt><dd class="m-0 text-cp-text">
                  {{ state.observed.bytes }} 字节
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  观测时间
                </dt><dd class="m-0 text-cp-text">
                  {{ formatDateTime(state.observed.observedAt) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  Transport
                </dt><dd class="m-0 break-all text-cp-text">
                  {{ state.observed.transport }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  SHA-256
                </dt><dd class="m-0 break-all font-mono text-cp-text">
                  {{ state.observed.sha256 }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  上游响应 ID
                </dt><dd class="m-0 break-all font-mono text-cp-text">
                  {{ state.observed.upstreamResponseId || '未提供' }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  客户端 Turn ID
                </dt><dd class="m-0 break-all font-mono text-cp-text">
                  {{ state.observed.clientTurnId || '未提供' }}
                </dd>
              </div>
            </dl>
            <BaseButton size="sm" class="justify-self-start" :disabled="saving" :loading="saving" @click="adoptObserved">
              采用当前观测并启用覆盖
            </BaseButton>
          </template>
          <p v-else class="m-0 text-cp-text-secondary">
            暂无真实上游观测；不会从客户端或本地缓存推断值。
          </p>
        </section>

        <section class="grid min-w-0 gap-4" aria-label="手动覆盖">
          <div class="flex flex-wrap items-center justify-between gap-3">
            <div>
              <h3 class="m-0 text-cp font-bold text-cp-text">
                手动覆盖
              </h3>
              <p class="mt-1 mb-0 text-xs text-cp-text-secondary">
                默认关闭。启停独立于账号的调度开关。
              </p>
            </div>
            <BaseSwitch v-model="draftEnabled" label="启用 Turn State 实验覆盖" :disabled="saving" active-text="启用" inactive-text="停用" />
          </div>
          <BaseFormItem label="覆盖值" description="编辑的是待保存草稿；关闭弹窗或切换账号即丢弃未保存内容。" :error="validationError">
            <div class="flex min-w-0 flex-wrap items-center gap-2">
              <BaseInput v-model="draftValue" :type="showOverride ? 'text' : 'password'" autocomplete="off" placeholder="输入不透明 Turn State 值" :disabled="saving" class="min-w-40 flex-1" />
              <BaseButton size="sm" :aria-label="showOverride ? '隐藏覆盖值' : '显示覆盖值'" @click="showOverride = !showOverride">
                <template #icon>
                  <EyeOff v-if="showOverride" :size="14" /><Eye v-else :size="14" />
                </template>
                {{ showOverride ? '隐藏' : '显示' }}
              </BaseButton>
              <BaseButton size="sm" :disabled="!draftValue" @click="copyText(draftValue, { successText: '覆盖草稿已复制' })">
                <template #icon>
                  <Copy :size="14" />
                </template>
                复制
              </BaseButton>
            </div>
          </BaseFormItem>
          <dl class="m-0 grid gap-x-4 gap-y-2 text-xs sm:grid-cols-3">
            <div>
              <dt class="text-cp-text-quaternary">
                已保存长度
              </dt><dd class="m-0 text-cp-text">
                {{ state.override.bytes }} 字节
              </dd>
            </div>
            <div>
              <dt class="text-cp-text-quaternary">
                更新时间
              </dt><dd class="m-0 text-cp-text">
                {{ state.override.updatedAt ? formatDateTime(state.override.updatedAt) : '未提供' }}
              </dd>
            </div>
            <div>
              <dt class="text-cp-text-quaternary">
                已保存 SHA-256
              </dt><dd class="m-0 break-all font-mono text-cp-text">
                {{ state.override.sha256 || '未提供' }}
              </dd>
            </div>
          </dl>
        </section>
        <p v-if="actionError" role="alert" class="m-0 text-cp-error-text">
          {{ actionError }}
        </p>
      </template>
    </div>
    <template #footer>
      <BaseButton variant="secondary" :disabled="saving" @click="open = false">
        关闭
      </BaseButton>
      <BaseButton variant="destructive" :disabled="!state || loading || saving || (!state.override.value && !state.override.enabled)" @click="clear">
        清空覆盖
      </BaseButton>
      <BaseButton variant="primary" :loading="saving" :disabled="!state || loading || saving || !changed || !!validationError" @click="save">
        保存覆盖
      </BaseButton>
    </template>
  </BaseModal>
</template>
