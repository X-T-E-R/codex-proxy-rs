<script setup lang="ts">
import type {
  AccountModelTurnStateResponse,
  ModelTurnStateCaptureAccepted,
  ModelTurnStateCaptureJob,
  ModelTurnStateCaptureStatus,
  OutboundProxyRecord,
} from '@/api'

import { Copy, Eye, EyeOff, RefreshCw } from '@lucide/vue'
import { useEventListener } from '@vueuse/core'
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import {
  cancelAccountModelTurnStateCapture,
  getAccountModels,
  getAccountModelTurnState,
  getAccountModelTurnStateCapture,
  getProxies,
  startAccountModelTurnStateCapture,
  updateAccountModelTurnState,
} from '@/api'
import { ApiError } from '@/api/request'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseConfirmModal from '@/components/base/BaseConfirmModal.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseNumberInput from '@/components/base/BaseNumberInput.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'
import { toast } from '@/components/base/BaseToast'
import { useCopyText } from '@/composables/useCopyText'
import { errorMessage } from '@/utils/async'
import { formatDateTime } from '@/utils/date'

type PinAction = 'keep' | 'replace' | 'clear' | 'invalidate'
type CaptureJobState = ModelTurnStateCaptureAccepted | ModelTurnStateCaptureJob

const props = defineProps<{
  accountId: string
  open: boolean
}>()
const emit = defineEmits<{
  busyChange: [busy: boolean]
}>()

const ACTIVE_CAPTURE_STATUSES: ModelTurnStateCaptureStatus[] = ['queued', 'running']
const POLL_INTERVAL_MS = 2_000
const MAX_POLL_FAILURES = 3

const copyText = useCopyText()
const models = ref<Array<{ id: string, label: string }>>([])
const proxies = ref<OutboundProxyRecord[]>([])
const selectedModel = ref('')
const modelState = ref<AccountModelTurnStateResponse | null>(null)
const captureJob = ref<CaptureJobState | null>(null)
const modelsLoading = ref(false)
const proxiesLoading = ref(false)
const modelLoading = ref(false)
const modelSaving = ref(false)
const captureStarting = ref(false)
const captureCancelling = ref(false)
const pollingStopped = ref(false)
const modelsError = ref('')
const proxiesError = ref('')
const modelError = ref('')
const actionError = ref('')
const captureError = ref('')
const showPin = ref(false)
const showPinDraft = ref(false)
const captureConfirmOpen = ref(false)

const draftLockEnabled = ref(false)
const draftCaptureEnabled = ref(false)
const draftReuseWindowSeconds = ref(3_600)
const draftCaptureProxyId = ref('')
const draftMaxAttempts = ref(3)
const draftAttemptTimeoutSeconds = ref(8)
const draftJobTimeoutSeconds = ref(30)
const draftBackoffSeconds = ref(1)
const draftMaxBackoffSeconds = ref(4)
const draftCooldownSeconds = ref(900)
const pinAction = ref<PinAction>('keep')
const pinDraftValue = ref('')

let generation = 0
let modelGeneration = 0
let pollTimer: number | undefined
let pollDeadline = 0
let pollFailures = 0

const busy = computed(() => modelSaving.value || captureStarting.value || captureCancelling.value)
const captureActive = computed(() => Boolean(
  captureJob.value && ACTIVE_CAPTURE_STATUSES.includes(captureJob.value.status),
))
const captureEditingLocked = computed(() => captureActive.value && !pollingStopped.value)
const pinDraftBytes = computed(() => new TextEncoder().encode(pinDraftValue.value).length)
const modelOptions = computed(() => models.value.map(model => ({
  label: model.label,
  value: model.id,
  description: model.id === model.label ? undefined : model.id,
})))
const proxyOptions = computed(() => {
  const options = proxies.value.map(proxy => ({
    label: `${proxy.name}${proxy.lastTest?.success ? '' : proxy.lastTest ? '（测试失败）' : '（未测试）'}`,
    value: proxy.id,
    description: proxy.endpoint,
    disabled: proxy.lastTest?.success !== true,
  }))
  const current = modelState.value?.captureProxy
  if (current && !options.some(option => option.value === current.id)) {
    options.push({
      label: `${current.name}（测试结果未载入）`,
      value: current.id,
      description: current.endpoint,
      disabled: true,
    })
  }
  return [
    { label: '未选择', value: '', description: '关闭自动捕获时可以不选择代理' },
    ...options,
  ]
})
const selectedProxyReady = computed(() => {
  if (!draftCaptureProxyId.value)
    return false
  const proxy = proxies.value.find(item => item.id === draftCaptureProxyId.value)
  return proxy?.lastTest?.success === true
})
const modelChanged = computed(() => {
  const current = modelState.value
  return Boolean(current && (
    draftLockEnabled.value !== current.lockEnabled
    || draftCaptureEnabled.value !== current.captureEnabled
    || draftReuseWindowSeconds.value !== current.reuseWindowSeconds
    || draftCaptureProxyId.value !== (current.captureProxyId ?? '')
    || draftMaxAttempts.value !== current.capturePolicy.maxAttempts
    || draftAttemptTimeoutSeconds.value !== current.capturePolicy.attemptTimeoutSeconds
    || draftJobTimeoutSeconds.value !== current.capturePolicy.jobTimeoutSeconds
    || draftBackoffSeconds.value !== current.capturePolicy.backoffSeconds
    || draftMaxBackoffSeconds.value !== current.capturePolicy.maxBackoffSeconds
    || draftCooldownSeconds.value !== current.capturePolicy.cooldownSeconds
    || pinAction.value !== 'keep'
  ))
})
const pinValidationError = computed(() => {
  if (pinAction.value === 'replace') {
    if (pinDraftBytes.value !== 292)
      return `手动值必须正好为 292 个 UTF-8 字节；当前 ${pinDraftBytes.value} 字节`
    if (/[^\x20-\x7E]/.test(pinDraftValue.value))
      return '手动值仅支持可打印 ASCII 字符，不含换行或控制字符'
  }
  if (draftLockEnabled.value) {
    if (pinAction.value === 'clear' || pinAction.value === 'invalidate')
      return '关闭模型锁定后，才能清除或标记当前值失效'
    if (pinAction.value === 'keep' && modelState.value?.pin?.encodedBytes !== 292)
      return '启用模型锁定前，需要保存一个 292 字节的值'
  }
  return ''
})
const settingsValidationError = computed(() => {
  if (draftCaptureEnabled.value && proxiesLoading.value)
    return '正在核对捕获代理的测试状态'
  if (draftCaptureEnabled.value && !selectedProxyReady.value)
    return '启用自动捕获前，请选择一个测试成功的已管理代理'
  if (draftAttemptTimeoutSeconds.value > draftJobTimeoutSeconds.value)
    return '单次尝试超时不能大于任务总超时'
  if (draftBackoffSeconds.value > draftMaxBackoffSeconds.value)
    return '首次退避不能大于最大退避'
  return pinValidationError.value
})
const canStartCapture = computed(() => Boolean(
  modelState.value
  && modelState.value.captureProxyId
  && selectedProxyReady.value
  && !modelChanged.value
  && !settingsValidationError.value
  && !captureActive.value
  && !busy.value,
))
const captureStatusView = computed(() => {
  const status = captureJob.value?.status
  const views: Record<ModelTurnStateCaptureStatus, { label: string, class: string }> = {
    queued: { label: '排队中', class: 'bg-cp-info-container text-cp-info-on-container' },
    running: { label: '获取中', class: 'bg-cp-info-container text-cp-info-on-container' },
    succeeded: { label: '已获取', class: 'bg-cp-success-container text-cp-success-on-container' },
    failed: { label: '获取失败', class: 'bg-cp-error-container text-cp-error-on-container' },
    cancelled: { label: '已取消', class: 'bg-cp-fill-tertiary text-cp-text-secondary' },
  }
  return status ? views[status] : null
})
const captureJobDetail = computed(() => {
  const job = captureJob.value
  return job && 'attempts' in job ? job : null
})

watch(busy, value => emit('busyChange', value), { immediate: true })

function stopPolling() {
  if (pollTimer !== undefined) {
    window.clearTimeout(pollTimer)
    pollTimer = undefined
  }
}

function resetModel() {
  modelGeneration++
  stopPolling()
  modelState.value = null
  captureJob.value = null
  modelError.value = ''
  actionError.value = ''
  captureError.value = ''
  showPin.value = false
  showPinDraft.value = false
  pinDraftValue.value = ''
  pinAction.value = 'keep'
  modelLoading.value = false
  modelSaving.value = false
  captureStarting.value = false
  captureCancelling.value = false
  pollingStopped.value = false
  captureConfirmOpen.value = false
  pollDeadline = 0
  pollFailures = 0
}

function reset() {
  generation++
  resetModel()
  models.value = []
  proxies.value = []
  selectedModel.value = ''
  modelsError.value = ''
  proxiesError.value = ''
  modelsLoading.value = false
  proxiesLoading.value = false
}

function isCaptureActive(job: CaptureJobState | null) {
  return Boolean(job && ACTIVE_CAPTURE_STATUSES.includes(job.status))
}

function schedulePoll() {
  stopPolling()
  if (!props.open || !isCaptureActive(captureJob.value))
    return
  if (document.hidden)
    return
  if (Date.now() >= pollDeadline) {
    pollingStopped.value = true
    captureError.value = '任务状态轮询已停止，设置编辑已解锁。刷新模型状态可继续查看结果。'
    return
  }
  const requestGeneration = generation
  const requestModelGeneration = modelGeneration
  const jobId = captureJob.value!.jobId
  pollTimer = window.setTimeout(async () => {
    pollTimer = undefined
    if (!props.open || requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    try {
      const result = await getAccountModelTurnStateCapture({ jobId })
      if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
        return
      captureJob.value = result
      captureError.value = ''
      pollFailures = 0
      pollingStopped.value = false
      if (isCaptureActive(result))
        schedulePoll()
      else
        await loadModelState()
    }
    catch (error) {
      if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
        return
      pollFailures += 1
      const message = errorMessage(error, '无法刷新获取任务状态')
      if (pollFailures < MAX_POLL_FAILURES) {
        captureError.value = message
        schedulePoll()
      }
      else {
        pollingStopped.value = true
        captureError.value = `${message}。连续刷新失败，轮询已停止；设置编辑已解锁。`
      }
    }
  }, POLL_INTERVAL_MS)
}

function applyModelState(result: AccountModelTurnStateResponse) {
  stopPolling()
  modelState.value = result
  captureJob.value = result.capture
  draftLockEnabled.value = result.lockEnabled
  draftCaptureEnabled.value = result.captureEnabled
  draftReuseWindowSeconds.value = result.reuseWindowSeconds
  draftCaptureProxyId.value = result.captureProxyId ?? ''
  draftMaxAttempts.value = result.capturePolicy.maxAttempts
  draftAttemptTimeoutSeconds.value = result.capturePolicy.attemptTimeoutSeconds
  draftJobTimeoutSeconds.value = result.capturePolicy.jobTimeoutSeconds
  draftBackoffSeconds.value = result.capturePolicy.backoffSeconds
  draftMaxBackoffSeconds.value = result.capturePolicy.maxBackoffSeconds
  draftCooldownSeconds.value = result.capturePolicy.cooldownSeconds
  pinAction.value = 'keep'
  pinDraftValue.value = ''
  showPin.value = false
  showPinDraft.value = false
  pollingStopped.value = false
  if (isCaptureActive(result.capture)) {
    pollDeadline = Date.now() + (result.capturePolicy.jobTimeoutSeconds + 30) * 1_000
    pollFailures = 0
    schedulePoll()
  }
}

async function loadModels() {
  const requestGeneration = generation
  modelsLoading.value = true
  modelsError.value = ''
  try {
    const result = await getAccountModels({ accountId: props.accountId })
    if (requestGeneration !== generation)
      return
    models.value = result.models
    if (!selectedModel.value && result.models[0])
      selectedModel.value = result.models[0].id
  }
  catch (error) {
    if (requestGeneration === generation)
      modelsError.value = errorMessage(error, '模型列表加载失败')
  }
  finally {
    if (requestGeneration === generation)
      modelsLoading.value = false
  }
}

async function loadProxies() {
  const requestGeneration = generation
  proxiesLoading.value = true
  proxiesError.value = ''
  try {
    const first = await getProxies({ page: 1, pageSize: 200 })
    const items = [...first.items]
    for (let page = 2; page <= first.page.totalPages; page += 1) {
      const result = await getProxies({ page, pageSize: 200 })
      if (requestGeneration !== generation)
        return
      items.push(...result.items)
    }
    if (requestGeneration === generation)
      proxies.value = items
  }
  catch (error) {
    if (requestGeneration === generation)
      proxiesError.value = errorMessage(error, '代理列表加载失败')
  }
  finally {
    if (requestGeneration === generation)
      proxiesLoading.value = false
  }
}

async function loadModelState() {
  if (!props.open || !selectedModel.value)
    return
  const requestGeneration = generation
  const requestModelGeneration = modelGeneration
  modelLoading.value = true
  modelError.value = ''
  actionError.value = ''
  captureError.value = ''
  try {
    const result = await getAccountModelTurnState({
      accountId: props.accountId,
      model: selectedModel.value,
    })
    if (requestGeneration === generation && requestModelGeneration === modelGeneration)
      applyModelState(result)
  }
  catch (error) {
    if (requestGeneration === generation && requestModelGeneration === modelGeneration)
      modelError.value = errorMessage(error, '模型 Turn State 读取失败')
  }
  finally {
    if (requestGeneration === generation && requestModelGeneration === modelGeneration)
      modelLoading.value = false
  }
}

watch([() => props.open, () => props.accountId], () => {
  reset()
  if (props.open) {
    void loadModels()
    void loadProxies()
  }
}, { immediate: true })

watch(selectedModel, (model, previous) => {
  if (model === previous)
    return
  resetModel()
  if (props.open && model)
    void loadModelState()
})

useEventListener(document, 'visibilitychange', () => {
  if (document.hidden)
    stopPolling()
  else
    schedulePoll()
})

function setPinAction(action: PinAction) {
  pinAction.value = action
  pinDraftValue.value = ''
  showPinDraft.value = false
}

function formatTokenVersion(version: number | null) {
  return version === null ? '未识别' : `0x${version.toString(16).padStart(2, '0')}`
}

function formatByteCount(value: number | null) {
  return value === null ? '未识别' : `${value} 字节`
}

async function handleConflict(accountId: string, model: string) {
  const discardedPinDraft = pinAction.value === 'replace' && Boolean(pinDraftValue.value)
  await loadModelState()
  if (!props.open || props.accountId !== accountId || selectedModel.value !== model)
    return
  actionError.value = modelError.value
    ? `配置已变化，刷新失败：${modelError.value}`
    : `配置已变化，已载入最新状态；请重新确认后保存。${discardedPinDraft ? '手动值草稿已清除。' : ''}`
}

async function handleCaptureConflict(accountId: string, model: string) {
  await loadModelState()
  if (!props.open || props.accountId !== accountId || selectedModel.value !== model)
    return
  captureError.value = modelError.value
    ? `任务状态刷新失败：${modelError.value}`
    : captureActive.value
      ? '已有获取任务，已载入最新状态。'
      : '模型配置已变化，已载入最新状态；请重新确认后启动任务。'
}

async function saveModelSettings() {
  const current = modelState.value
  if (!current || !modelChanged.value || settingsValidationError.value || captureEditingLocked.value || busy.value)
    return
  const requestGeneration = generation
  const requestModelGeneration = modelGeneration
  modelSaving.value = true
  actionError.value = ''
  try {
    const result = await updateAccountModelTurnState({
      accountId: current.accountId,
      model: current.requestedModel,
      expectedRevision: current.configRevision,
      expectedIdentityRevision: current.identityRevision,
      expectedEffectiveModel: current.effectiveModel,
      lockEnabled: draftLockEnabled.value,
      captureEnabled: draftCaptureEnabled.value,
      reuseWindowSeconds: draftReuseWindowSeconds.value,
      captureProxyId: draftCaptureProxyId.value || null,
      maxAttempts: draftMaxAttempts.value,
      attemptTimeoutSeconds: draftAttemptTimeoutSeconds.value,
      jobTimeoutSeconds: draftJobTimeoutSeconds.value,
      backoffSeconds: draftBackoffSeconds.value,
      maxBackoffSeconds: draftMaxBackoffSeconds.value,
      cooldownSeconds: draftCooldownSeconds.value,
      pinAction: pinAction.value,
      ...(pinAction.value === 'replace' ? { value: pinDraftValue.value } : {}),
    })
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    applyModelState(result)
    toast.success('模型 Turn State 设置已保存')
  }
  catch (error) {
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    if (error instanceof ApiError && error.status === 409)
      await handleConflict(current.accountId, current.requestedModel)
    else
      actionError.value = errorMessage(error, '模型 Turn State 设置保存失败')
  }
  finally {
    if (requestGeneration === generation && requestModelGeneration === modelGeneration)
      modelSaving.value = false
  }
}

function requestCapture() {
  if (canStartCapture.value)
    captureConfirmOpen.value = true
}

async function startCapture() {
  const current = modelState.value
  if (!current || !canStartCapture.value)
    return
  const requestGeneration = generation
  const requestModelGeneration = modelGeneration
  captureConfirmOpen.value = false
  captureStarting.value = true
  captureError.value = ''
  try {
    const result = await startAccountModelTurnStateCapture({
      accountId: current.accountId,
      model: current.requestedModel,
      expectedRevision: current.configRevision,
      expectedIdentityRevision: current.identityRevision,
      expectedEffectiveModel: current.effectiveModel,
    })
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    captureJob.value = result
    pollingStopped.value = false
    pollDeadline = Date.now() + (current.capturePolicy.jobTimeoutSeconds + 30) * 1_000
    pollFailures = 0
    toast.success('Turn State 获取任务已创建')
    schedulePoll()
  }
  catch (error) {
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    if (error instanceof ApiError && error.status === 409)
      await handleCaptureConflict(current.accountId, current.requestedModel)
    else
      captureError.value = errorMessage(error, 'Turn State 获取任务创建失败')
  }
  finally {
    if (requestGeneration === generation && requestModelGeneration === modelGeneration)
      captureStarting.value = false
  }
}

async function cancelCapture() {
  const job = captureJob.value
  if (!job || !captureActive.value || captureCancelling.value)
    return
  const requestGeneration = generation
  const requestModelGeneration = modelGeneration
  captureCancelling.value = true
  captureError.value = ''
  try {
    const result = await cancelAccountModelTurnStateCapture({ jobId: job.jobId })
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    captureJob.value = result
    stopPolling()
    await loadModelState()
    toast.success('已请求取消 Turn State 获取任务')
  }
  catch (error) {
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    if (error instanceof ApiError && error.status === 409) {
      await loadModelState()
      if (!modelError.value)
        captureError.value = '任务状态已变化，已刷新最新结果。'
    }
    else {
      captureError.value = errorMessage(error, 'Turn State 获取任务取消失败')
    }
  }
  finally {
    if (requestGeneration === generation && requestModelGeneration === modelGeneration)
      captureCancelling.value = false
  }
}

onBeforeUnmount(() => {
  stopPolling()
  emit('busyChange', false)
})
</script>

<template>
  <section class="grid min-w-0 gap-4 border-t border-cp-split pt-5" aria-label="模型级锁定与捕获">
    <div class="flex flex-wrap items-start justify-between gap-3">
      <div>
        <h3 class="m-0 text-cp-lg font-bold text-cp-text">
          模型级锁定与捕获
        </h3>
        <p class="mt-1 mb-0 text-xs leading-relaxed text-cp-text-secondary">
          每个账号和实际上游模型独立保存。跨 turn 锁定属于实验行为，只用于 HTTP transport，不改变账号调度。
        </p>
      </div>
      <BaseButton
        size="sm"
        variant="ghost"
        :disabled="modelsLoading || modelLoading || busy || modelChanged"
        title="保存或放弃草稿后刷新"
        @click="loadModelState"
      >
        <template #icon>
          <RefreshCw :size="14" />
        </template>
        刷新模型状态
      </BaseButton>
    </div>

    <div v-if="modelsLoading" role="status" class="rounded-cp bg-cp-fill-quaternary p-4 text-cp-text-secondary">
      正在加载账号模型…
    </div>
    <div v-else-if="modelsError" role="alert" class="grid gap-3 rounded-cp bg-cp-error-container p-4 text-cp-error-on-container">
      <span>{{ modelsError }}</span>
      <BaseButton size="sm" class="justify-self-start" @click="loadModels">
        重试
      </BaseButton>
    </div>
    <div v-else-if="models.length === 0" class="rounded-cp bg-cp-fill-quaternary p-4 text-cp-text-secondary">
      该账号没有可配置的模型。模型目录返回数据后可在此设置锁定与捕获。
    </div>
    <template v-else>
      <BaseFormItem label="目标模型" description="选择账号模型目录中的请求模型；服务端会显示最终映射到的实际上游模型。">
        <BaseSelect
          v-model="selectedModel"
          :options="modelOptions"
          :disabled="modelLoading || busy"
          empty-text="账号没有可用模型"
          aria-label="目标模型"
        />
      </BaseFormItem>

      <p v-if="modelLoading" role="status" class="m-0 rounded-cp bg-cp-fill-quaternary p-4 text-cp-text-secondary">
        正在读取模型 Turn State…
      </p>
      <div v-else-if="modelError" role="alert" class="grid gap-3 rounded-cp bg-cp-error-container p-4 text-cp-error-on-container">
        <span>{{ modelError }}</span>
        <BaseButton size="sm" class="justify-self-start" @click="loadModelState">
          重试
        </BaseButton>
      </div>
      <template v-else-if="modelState">
        <div class="grid gap-2 rounded-cp bg-cp-fill-quaternary p-4 text-xs sm:grid-cols-2">
          <div class="min-w-0">
            <p class="m-0 text-cp-text-quaternary">
              请求模型 → 实际上游模型
            </p>
            <p class="mt-1 mb-0 break-all font-mono text-cp-text">
              {{ modelState.requestedModel }} → {{ modelState.effectiveModel }}
            </p>
          </div>
          <div>
            <p class="m-0 text-cp-text-quaternary">
              账号身份代次
            </p>
            <p class="mt-1 mb-0 font-mono text-cp-text">
              {{ modelState.identityRevision }}
            </p>
          </div>
        </div>

        <div class="grid gap-4 rounded-cp bg-cp-fill-quaternary p-4 sm:grid-cols-2">
          <div class="flex items-start justify-between gap-3">
            <div>
              <p class="m-0 font-bold text-cp-text">
                模型锁定
              </p>
              <p class="mt-1 mb-0 text-xs leading-relaxed text-cp-text-secondary">
                实验性地跨 turn 注入当前可用值；aged 值已停止注入。
              </p>
            </div>
            <BaseSwitch v-model="draftLockEnabled" label="启用模型 Turn State 锁定" :disabled="busy || captureEditingLocked" active-text="启用" inactive-text="停用" />
          </div>
          <div class="flex items-start justify-between gap-3">
            <div>
              <p class="m-0 font-bold text-cp-text">
                自动捕获
              </p>
              <p class="mt-1 mb-0 text-xs leading-relaxed text-cp-text-secondary">
                按下方策略通过已测试代理获取值。
              </p>
            </div>
            <BaseSwitch v-model="draftCaptureEnabled" label="启用模型 Turn State 自动捕获" :disabled="busy || captureEditingLocked" active-text="启用" inactive-text="停用" />
          </div>
        </div>

        <div class="grid gap-4 sm:grid-cols-2">
          <BaseFormItem control-id="model-turn-state-reuse-window" label="本地最大复用窗口" description="这是网关允许复用保存值的本地上限，不是上游公布或保证的真实 TTL。">
            <BaseNumberInput id="model-turn-state-reuse-window" v-model="draftReuseWindowSeconds" aria-describedby="model-turn-state-reuse-window-description" label="本地最大复用窗口" :min="1" :max="86400" unit="秒" :disabled="busy || captureEditingLocked" />
          </BaseFormItem>
          <BaseFormItem
            label="捕获代理"
            description="选择已管理且测试成功的代理。DataImpulse 等服务可先把 rotating URL 添加为代理并完成测试。"
            :error="draftCaptureEnabled && !proxiesLoading && !selectedProxyReady ? '自动捕获需要测试成功的代理' : undefined"
          >
            <BaseSelect
              v-model="draftCaptureProxyId"
              :options="proxyOptions"
              :disabled="proxiesLoading || busy || captureEditingLocked"
              :placeholder="proxiesLoading ? '加载代理中…' : '选择捕获代理'"
              empty-text="还没有已管理代理"
              aria-label="捕获代理"
            />
            <p v-if="!proxiesLoading && !proxiesError && proxies.length === 0" class="mt-2 mb-0 text-xs text-cp-text-quaternary">
              还没有已管理代理。先在代理页面添加并测试代理，再返回这里选择。
            </p>
            <div v-if="proxiesError" role="alert" class="mt-2 flex flex-wrap items-center gap-2 text-xs text-cp-error-text">
              <span>{{ proxiesError }}</span>
              <BaseButton size="sm" variant="ghost" @click="loadProxies">
                重试
              </BaseButton>
            </div>
          </BaseFormItem>
        </div>

        <section class="grid min-w-0 gap-3 rounded-cp bg-cp-bg-container p-4 shadow-cp-tertiary" aria-label="模型锁定值">
          <div class="flex flex-wrap items-center justify-between gap-2">
            <div>
              <h4 class="m-0 font-bold text-cp-text">
                模型锁定值
              </h4>
              <p class="mt-1 mb-0 text-xs text-cp-text-secondary">
                保存值必须正好为 292 个 UTF-8 字节，并且只能包含可打印 ASCII 字符。长度规则命中不说明值有效、正常或模型质量；Fernet 字段只解析公开外层结构，不验证密文或 HMAC。
              </p>
            </div>
            <span
              v-if="modelState.pin"
              class="rounded-full px-2.5 py-1 text-xs font-bold"
              :class="modelState.pin.status === 'fresh' ? 'bg-cp-success-container text-cp-success-on-container' : 'bg-cp-warning-container text-cp-warning-on-container'"
            >
              {{ modelState.pin.status === 'fresh' ? 'fresh · 本地窗口内' : 'aged · 已停止注入' }}
            </span>
          </div>
          <template v-if="modelState.pin">
            <div class="flex min-w-0 flex-wrap items-center gap-2">
              <BaseInput :model-value="modelState.pin.value" :type="showPin ? 'text' : 'password'" readonly aria-label="已保存的模型 Turn State 值" class="min-w-40 flex-1" />
              <BaseButton size="sm" :aria-label="showPin ? '隐藏已保存值' : '显示已保存值'" @click="showPin = !showPin">
                <template #icon>
                  <EyeOff v-if="showPin" :size="14" /><Eye v-else :size="14" />
                </template>
                {{ showPin ? '隐藏' : '显示' }}
              </BaseButton>
              <BaseButton size="sm" @click="copyText(modelState.pin!.value, { successText: '模型 Turn State 值已复制' })">
                <template #icon>
                  <Copy :size="14" />
                </template>
                复制
              </BaseButton>
            </div>
            <dl class="m-0 grid gap-x-4 gap-y-2 text-xs sm:grid-cols-3">
              <div>
                <dt class="text-cp-text-quaternary">
                  来源
                </dt><dd class="m-0 text-cp-text">
                  {{ modelState.pin.source === 'manual' ? '手动保存' : '捕获任务' }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  捕获时间
                </dt><dd class="m-0 text-cp-text">
                  {{ formatDateTime(modelState.pin.capturedAt) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  本地复用截止
                </dt><dd class="m-0 text-cp-text">
                  {{ formatDateTime(modelState.pin.reuseDeadline) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  长度
                </dt><dd class="m-0 text-cp-text">
                  {{ modelState.pin.encodedBytes }} 字节 · {{ modelState.pin.encodedBytes === 292 ? '规则命中' : '规则未命中' }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  解码后长度
                </dt><dd class="m-0 text-cp-text">
                  {{ formatByteCount(modelState.pin.decodedBytes ?? modelState.pin.rawBytes) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  Ciphertext 长度
                </dt><dd class="m-0 text-cp-text">
                  {{ formatByteCount(modelState.pin.ciphertextBytes) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  外层格式
                </dt><dd class="m-0 text-cp-text">
                  {{ modelState.pin.envelopeFormat === 'fernet_v0x80_candidate' ? 'Fernet v0x80 candidate' : '未识别' }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  Token version
                </dt><dd class="m-0 font-mono text-cp-text">
                  {{ formatTokenVersion(modelState.pin.tokenVersion) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  内嵌生成时间（未验证）
                </dt><dd class="m-0 text-cp-text">
                  {{ modelState.pin.issuedAt ? formatDateTime(modelState.pin.issuedAt) : '未识别' }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  兼容 transport
                </dt><dd class="m-0 text-cp-text">
                  HTTP only
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  SHA-256
                </dt><dd class="m-0 break-all font-mono text-cp-text">
                  {{ modelState.pin.sha256 }}
                </dd>
              </div>
            </dl>
          </template>
          <p v-else class="m-0 text-cp-text-secondary">
            当前模型还没有保存值。可以手动替换，或配置代理后启动获取任务。
          </p>

          <div class="flex flex-wrap gap-2">
            <BaseButton size="sm" :variant="pinAction === 'replace' ? 'primary' : 'secondary'" :disabled="busy || captureEditingLocked" @click="setPinAction('replace')">
              手动替换
            </BaseButton>
            <BaseButton size="sm" :variant="pinAction === 'invalidate' ? 'primary' : 'secondary'" :disabled="!modelState.pin || busy || captureEditingLocked" @click="setPinAction('invalidate')">
              标记失效
            </BaseButton>
            <BaseButton size="sm" :variant="pinAction === 'clear' ? 'destructive' : 'secondary'" :disabled="!modelState.pin || busy || captureEditingLocked" @click="setPinAction('clear')">
              清除保存值
            </BaseButton>
            <BaseButton v-if="pinAction !== 'keep'" size="sm" variant="ghost" :disabled="busy" @click="setPinAction('keep')">
              取消值操作
            </BaseButton>
          </div>
          <BaseFormItem v-if="pinAction === 'replace'" label="新的模型 Turn State 值" :error="pinValidationError">
            <div class="flex min-w-0 flex-wrap items-center gap-2">
              <BaseInput v-model="pinDraftValue" :type="showPinDraft ? 'text' : 'password'" autocomplete="off" placeholder="输入 292 字节的值" :disabled="busy" class="min-w-40 flex-1" />
              <BaseButton size="sm" :aria-label="showPinDraft ? '隐藏手动值' : '显示手动值'" @click="showPinDraft = !showPinDraft">
                <template #icon>
                  <EyeOff v-if="showPinDraft" :size="14" /><Eye v-else :size="14" />
                </template>
                {{ showPinDraft ? '隐藏' : '显示' }}
              </BaseButton>
              <BaseButton size="sm" :disabled="!pinDraftValue" @click="copyText(pinDraftValue, { successText: '手动值草稿已复制' })">
                <template #icon>
                  <Copy :size="14" />
                </template>
                复制
              </BaseButton>
            </div>
            <p class="mt-2 mb-0 text-xs" :class="pinDraftBytes === 292 ? 'text-cp-info-text' : 'text-cp-text-quaternary'">
              当前 {{ pinDraftBytes }} / 292 UTF-8 字节 · {{ pinDraftBytes === 292 ? '长度规则命中' : '长度规则未命中' }}
            </p>
          </BaseFormItem>
          <p v-else-if="pinAction === 'invalidate'" role="status" class="m-0 text-xs text-cp-warning-text">
            保存后，这个值会保留用于审计，但不再作为可用的模型锁定值。
          </p>
          <p v-else-if="pinAction === 'clear'" role="status" class="m-0 text-xs text-cp-error-text">
            保存后会删除这个模型的当前值。
          </p>
          <p v-if="modelState.pin?.status === 'aged'" role="status" class="m-0 text-xs text-cp-warning-text">
            这个值已停止注入。可以启用自动捕获或使用“手动获取”触发刷新，也可以手动替换。
          </p>
        </section>

        <details class="group rounded-cp bg-cp-fill-quaternary">
          <summary class="cursor-pointer rounded-cp px-4 py-3 font-bold text-cp-text hover:bg-cp-bg-text-hover focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-cp-primary">
            捕获策略高级设置
          </summary>
          <div class="grid gap-4 px-4 pb-4 sm:grid-cols-2 lg:grid-cols-3">
            <BaseFormItem control-id="model-turn-state-max-attempts" label="最大尝试次数" description="1–10 次">
              <BaseNumberInput id="model-turn-state-max-attempts" v-model="draftMaxAttempts" aria-describedby="model-turn-state-max-attempts-description" label="最大尝试次数" :min="1" :max="10" unit="次" :disabled="busy || captureEditingLocked" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-attempt-timeout" label="单次尝试超时" description="1–60 秒，且不能大于任务总超时">
              <BaseNumberInput id="model-turn-state-attempt-timeout" v-model="draftAttemptTimeoutSeconds" aria-describedby="model-turn-state-attempt-timeout-description" label="单次尝试超时" :min="1" :max="60" unit="秒" :disabled="busy || captureEditingLocked" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-job-timeout" label="任务总超时" description="1–300 秒">
              <BaseNumberInput id="model-turn-state-job-timeout" v-model="draftJobTimeoutSeconds" aria-describedby="model-turn-state-job-timeout-description" label="任务总超时" :min="1" :max="300" unit="秒" :disabled="busy || captureEditingLocked" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-backoff" label="首次退避" description="0–60 秒">
              <BaseNumberInput id="model-turn-state-backoff" v-model="draftBackoffSeconds" aria-describedby="model-turn-state-backoff-description" label="首次退避" :min="0" :max="60" unit="秒" :disabled="busy || captureEditingLocked" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-max-backoff" label="最大退避" description="0–60 秒，不能小于首次退避">
              <BaseNumberInput id="model-turn-state-max-backoff" v-model="draftMaxBackoffSeconds" aria-describedby="model-turn-state-max-backoff-description" label="最大退避" :min="0" :max="60" unit="秒" :disabled="busy || captureEditingLocked" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-cooldown" label="失败冷却" description="0–86400 秒">
              <BaseNumberInput id="model-turn-state-cooldown" v-model="draftCooldownSeconds" aria-describedby="model-turn-state-cooldown-description" label="失败冷却" :min="0" :max="86400" unit="秒" :disabled="busy || captureEditingLocked" />
            </BaseFormItem>
          </div>
        </details>

        <div class="grid gap-3 rounded-cp bg-cp-fill-quaternary p-4">
          <div class="flex flex-wrap items-start justify-between gap-3">
            <div>
              <h4 class="m-0 font-bold text-cp-text">
                手动获取
              </h4>
              <p class="mt-1 mb-0 text-xs leading-relaxed text-cp-text-secondary">
                使用已保存的代理与策略创建一次后台获取任务。先保存上方草稿，再启动任务。
              </p>
            </div>
            <div class="flex flex-wrap gap-2">
              <BaseButton v-if="captureActive" size="sm" variant="secondary" :loading="captureCancelling" @click="cancelCapture">
                取消任务
              </BaseButton>
              <BaseButton size="sm" variant="primary" :loading="captureStarting" :disabled="!canStartCapture" @click="requestCapture">
                手动获取
              </BaseButton>
            </div>
          </div>
          <p v-if="!modelState.captureProxyId" class="m-0 text-xs text-cp-warning-text">
            保存一个测试成功的捕获代理后，才能启动手动获取。
          </p>
          <template v-if="captureJob">
            <div class="flex flex-wrap items-center gap-2">
              <span v-if="captureStatusView" class="rounded-full px-2.5 py-1 text-xs font-bold" :class="captureStatusView.class">
                {{ captureStatusView.label }}
              </span>
              <span class="break-all font-mono text-xs text-cp-text-secondary">{{ captureJob.jobId }}</span>
            </div>
            <dl v-if="captureJobDetail" class="m-0 grid gap-x-4 gap-y-2 text-xs sm:grid-cols-2 lg:grid-cols-4">
              <div>
                <dt class="text-cp-text-quaternary">
                  已尝试
                </dt><dd class="m-0 text-cp-text">
                  {{ captureJobDetail.attempts }} 次
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  创建时间
                </dt><dd class="m-0 text-cp-text">
                  {{ formatDateTime(captureJobDetail.createdAt) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  开始时间
                </dt><dd class="m-0 text-cp-text">
                  {{ captureJobDetail.startedAt ? formatDateTime(captureJobDetail.startedAt) : '尚未开始' }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  结束时间
                </dt><dd class="m-0 text-cp-text">
                  {{ captureJobDetail.finishedAt ? formatDateTime(captureJobDetail.finishedAt) : '尚未结束' }}
                </dd>
              </div>
            </dl>
            <p v-else class="m-0 text-xs text-cp-text-secondary">
              正在读取任务详情…
            </p>
            <p v-if="captureJobDetail?.reason" class="m-0 break-words text-xs text-cp-text-secondary">
              原因：{{ captureJobDetail.reason }}
            </p>
          </template>
          <p v-else class="m-0 text-xs text-cp-text-secondary">
            还没有获取任务。
          </p>
          <div v-if="captureError" role="alert" class="flex flex-wrap items-center justify-between gap-2 text-xs text-cp-error-text">
            <span>{{ captureError }}</span>
            <BaseButton v-if="captureActive" size="sm" variant="ghost" :disabled="busy" @click="loadModelState">
              {{ modelChanged ? '放弃草稿并刷新' : '刷新模型状态' }}
            </BaseButton>
          </div>
        </div>

        <div class="grid gap-2 rounded-cp bg-cp-bg-container p-4 text-xs">
          <p class="m-0 font-bold text-cp-text">
            Legacy fallback
          </p>
          <p class="m-0 text-cp-text-secondary">
            旧版账号覆盖：{{ modelState.legacyOverride.enabled ? '已启用' : '已停用' }}；{{ modelState.legacyOverride.configured ? '已配置值' : '未配置值' }}。
            {{ modelState.legacyOverride.willApplyWhenModelPinUnavailable ? '模型值不可用时会回退到旧版覆盖。' : '当前不会作为模型值不可用时的回退。' }}
          </p>
        </div>

        <p v-if="settingsValidationError" role="alert" class="m-0 text-cp-error-text">
          {{ settingsValidationError }}
        </p>
        <p v-if="actionError" role="alert" class="m-0 text-cp-error-text">
          {{ actionError }}
        </p>
        <div class="flex justify-end">
          <BaseButton
            variant="primary"
            :loading="modelSaving"
            :disabled="!modelChanged || !!settingsValidationError || captureEditingLocked || busy"
            @click="saveModelSettings"
          >
            保存模型设置
          </BaseButton>
        </div>
      </template>
    </template>
  </section>

  <BaseConfirmModal
    v-model="captureConfirmOpen"
    title="启动 Turn State 获取？"
    description="任务会通过已保存的捕获代理向上游发送请求。"
    confirm-text="开始获取"
    :loading="captureStarting"
    @confirm="startCapture"
  >
    <p class="m-0">
      收到任意 state 后会立即取消剩余尝试；已经发送的请求仍可能产生费用。
    </p>
  </BaseConfirmModal>
</template>
