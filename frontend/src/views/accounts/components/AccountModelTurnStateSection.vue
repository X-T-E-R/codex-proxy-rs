<script setup lang="ts">
import type {
  AccountModelTurnStateResponse,
  AccountTurnStatePolicyResponse,
  ModelTurnStateCaptureAccepted,
  ModelTurnStateCaptureJob,
  ModelTurnStateCaptureStatus,
  ModelTurnStateCaptureTriggerMode,
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
  getAccountTurnStatePolicy,
  getProxies,
  startAccountModelTurnStateCapture,
  updateAccountModelTurnState,
  updateAccountTurnStatePolicy,
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

type PinAction = 'keep' | 'replace' | 'clear' | 'invalidate' | 'importLegacy'
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
const CAPTURE_PROXY_TEST_MAX_AGE_MS = 24 * 60 * 60 * 1_000
const RECOMMENDED_REUSE_WINDOW_SECONDS = 7_200
const RECOMMENDED_REFRESH_LEAD_SECONDS = 900
const RECOMMENDED_CAPTURE_POLICY = {
  maxAttempts: 3,
  attemptTimeoutSeconds: 8,
  jobTimeoutSeconds: 30,
  backoffSeconds: 1,
  maxBackoffSeconds: 4,
  cooldownSeconds: 900,
} as const

const copyText = useCopyText()
const models = ref<Array<{ id: string, label: string }>>([])
const proxies = ref<OutboundProxyRecord[]>([])
const selectedModel = ref('')
const modelState = ref<AccountModelTurnStateResponse | null>(null)
const accountPolicy = ref<AccountTurnStatePolicyResponse | null>(null)
const captureJob = ref<CaptureJobState | null>(null)
const modelsLoading = ref(false)
const proxiesLoading = ref(false)
const modelLoading = ref(false)
const policyLoading = ref(false)
const modelSaving = ref(false)
const policySaving = ref(false)
const captureStarting = ref(false)
const captureCancelling = ref(false)
const pollingStopped = ref(false)
const modelsError = ref('')
const proxiesError = ref('')
const modelError = ref('')
const policyError = ref('')
const actionError = ref('')
const captureError = ref('')
const showPin = ref(false)
const showPinDraft = ref(false)
const showLegacy = ref(false)
const captureConfirmOpen = ref(false)

const draftLockEnabled = ref(false)
const draftCaptureEnabled = ref(false)
const draftReuseWindowSeconds = ref(7_200)
const draftRefreshLeadSeconds = ref(900)
const draftCaptureTriggerMode = ref<ModelTurnStateCaptureTriggerMode>('on_attributed_failure')
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

const busy = computed(() => modelSaving.value || policySaving.value || captureStarting.value || captureCancelling.value)
const captureActive = computed(() => Boolean(
  captureJob.value && ACTIVE_CAPTURE_STATUSES.includes(captureJob.value.status),
))
const pinDraftBytes = computed(() => new TextEncoder().encode(pinDraftValue.value).length)
const modelOptions = computed(() => models.value.map(model => ({
  label: model.label,
  value: model.id,
  description: model.id === model.label ? undefined : model.id,
})))
const captureTriggerOptions = [
  { label: '归因拒绝时捕获（默认）', value: 'on_attributed_failure', description: '只有当前值被实际发送，并被上游结构化拒绝时才轮换。' },
  { label: '用过后提前捕获', value: 'before_expiry_if_used', description: '当前值至少实际发送过一次后，才在到期前按提前量准备候选。' },
  { label: '到期后首请求捕获', value: 'first_request_after_expiry', description: '到期不后台探测；首个后续业务请求触发一次捕获。' },
  { label: '拒绝或到期首请求', value: 'failure_or_first_after_expiry', description: '结构化拒绝优先，否则由到期后的首个业务请求兜底。' },
] satisfies Array<{ label: string, value: ModelTurnStateCaptureTriggerMode, description: string }>
function proxyReady(proxy: OutboundProxyRecord) {
  if (proxy.lastTest?.success !== true || !proxy.lastTestAt)
    return false
  const testedAt = Date.parse(proxy.lastTestAt)
  return Number.isFinite(testedAt) && testedAt >= Date.now() - CAPTURE_PROXY_TEST_MAX_AGE_MS
}
const proxyOptions = computed(() => {
  const options = proxies.value.map(proxy => ({
    label: `${proxy.name}${proxyReady(proxy) ? '' : proxy.lastTest?.success ? '（测试已过期）' : proxy.lastTest ? '（测试失败）' : '（未测试）'}`,
    value: proxy.id,
    description: proxy.endpoint,
    disabled: false,
  }))
  const current = modelState.value?.captureProxy
  if (current && !options.some(option => option.value === current.id)) {
    options.push({
      label: `${current.name}（测试结果未载入）`,
      value: current.id,
      description: current.endpoint,
      disabled: false,
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
  return proxy !== undefined && proxyReady(proxy)
})
const readyProxies = computed(() => proxies.value.filter(proxyReady))
const captureProxyValidationError = computed(() => {
  if (!draftCaptureEnabled.value)
    return ''
  if (proxiesLoading.value)
    return '正在核对捕获代理的测试状态；账号策略仍可保存'
  if (proxiesError.value)
    return '代理列表读取失败；账号策略仍可保存，自动捕获会等待代理就绪'
  if (proxies.value.length === 0)
    return '自动捕获当前在等待代理；可先保存，再到代理管理添加并测试'
  if (readyProxies.value.length === 0)
    return '自动捕获当前在等待代理测试；普通请求不受影响'
  if (!draftCaptureProxyId.value)
    return '自动捕获当前未指定代理；可先保存，稍后再选择'
  if (!selectedProxyReady.value)
    return '所选代理尚未就绪；设置可以保存，自动捕获会等待最近 24 小时内的成功测试'
  return ''
})
const policyChanged = computed(() => {
  const current = accountPolicy.value
  return Boolean(current && (
    draftLockEnabled.value !== current.lockEnabled
    || draftCaptureEnabled.value !== current.captureEnabled
    || draftReuseWindowSeconds.value !== current.reuseWindowSeconds
    || draftRefreshLeadSeconds.value !== current.refreshLeadSeconds
    || draftCaptureTriggerMode.value !== current.captureTriggerMode
    || draftCaptureProxyId.value !== (current.captureProxyId ?? '')
    || draftMaxAttempts.value !== current.capturePolicy.maxAttempts
    || draftAttemptTimeoutSeconds.value !== current.capturePolicy.attemptTimeoutSeconds
    || draftJobTimeoutSeconds.value !== current.capturePolicy.jobTimeoutSeconds
    || draftBackoffSeconds.value !== current.capturePolicy.backoffSeconds
    || draftMaxBackoffSeconds.value !== current.capturePolicy.maxBackoffSeconds
    || draftCooldownSeconds.value !== current.capturePolicy.cooldownSeconds
  ))
})
const modelChanged = computed(() => pinAction.value !== 'keep')
const pinValidationError = computed(() => {
  if (pinAction.value === 'replace') {
    if (pinDraftBytes.value < 1 || pinDraftBytes.value > 16_384)
      return `手动值需为 1–16384 个 UTF-8 字节；当前 ${pinDraftBytes.value} 字节`
    if (/[^\x20-\x7E]/.test(pinDraftValue.value))
      return '手动值仅支持可打印 ASCII 字符，不含换行或控制字符'
  }
  return ''
})
const setupStatus = computed(() => {
  if (!draftLockEnabled.value)
    return '锁定未启用：请求不会注入模型级 Turn State。'
  if (modelState.value?.pin?.status === 'fresh')
    return '锁定已就绪：Responses HTTP 与 WebSocket 请求都会注入当前模型值。'
  if (draftCaptureEnabled.value && selectedProxyReady.value)
    return '待获取并自动锁定：先走账号正常出口；只有明确观测到非 292 字节值后，才使用所选代理捕获。'
  return '待获取并自动锁定：先走账号正常出口；观测到 292 字节值后会直接锁定。'
})
const canStartCapture = computed(() => Boolean(
  modelState.value
  && accountPolicy.value?.captureProxyId
  && selectedProxyReady.value
  && !policyError.value
  && !modelError.value
  && !policyChanged.value
  && !modelChanged.value
  && !pinValidationError.value
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
  showLegacy.value = false
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
  accountPolicy.value = null
  modelsError.value = ''
  policyError.value = ''
  proxiesError.value = ''
  modelsLoading.value = false
  policyLoading.value = false
  policySaving.value = false
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
  pinAction.value = 'keep'
  pinDraftValue.value = ''
  showPin.value = false
  showPinDraft.value = false
  showLegacy.value = false
  pollingStopped.value = false
  if (isCaptureActive(result.capture)) {
    pollDeadline = Date.now() + ((accountPolicy.value?.capturePolicy.jobTimeoutSeconds ?? 30) + 30) * 1_000
    pollFailures = 0
    schedulePoll()
  }
}

function applyPolicy(result: AccountTurnStatePolicyResponse) {
  accountPolicy.value = result
  draftLockEnabled.value = result.lockEnabled
  draftCaptureEnabled.value = result.captureEnabled
  draftReuseWindowSeconds.value = result.reuseWindowSeconds
  draftRefreshLeadSeconds.value = result.refreshLeadSeconds
  draftCaptureTriggerMode.value = result.captureTriggerMode
  draftCaptureProxyId.value = result.captureProxyId ?? ''
  draftMaxAttempts.value = result.capturePolicy.maxAttempts
  draftAttemptTimeoutSeconds.value = result.capturePolicy.attemptTimeoutSeconds
  draftJobTimeoutSeconds.value = result.capturePolicy.jobTimeoutSeconds
  draftBackoffSeconds.value = result.capturePolicy.backoffSeconds
  draftMaxBackoffSeconds.value = result.capturePolicy.maxBackoffSeconds
  draftCooldownSeconds.value = result.capturePolicy.cooldownSeconds
  chooseOnlyReadyProxy()
}

async function loadPolicy() {
  if (!props.open)
    return
  const requestGeneration = generation
  policyLoading.value = true
  policyError.value = ''
  try {
    const result = await getAccountTurnStatePolicy({ accountId: props.accountId })
    if (requestGeneration === generation)
      applyPolicy(result)
  }
  catch (error) {
    if (requestGeneration === generation)
      policyError.value = errorMessage(error, '账号 Turn State 策略读取失败')
  }
  finally {
    if (requestGeneration === generation)
      policyLoading.value = false
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
    if (requestGeneration === generation) {
      proxies.value = items
      chooseOnlyReadyProxy()
    }
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

function chooseOnlyReadyProxy() {
  if (draftCaptureEnabled.value && !draftCaptureProxyId.value && readyProxies.value.length === 1)
    draftCaptureProxyId.value = readyProxies.value[0]!.id
}

function handleCaptureToggle(enabled: boolean) {
  if (enabled)
    chooseOnlyReadyProxy()
}

function applyRecommendedDefaults() {
  draftReuseWindowSeconds.value = RECOMMENDED_REUSE_WINDOW_SECONDS
  draftRefreshLeadSeconds.value = RECOMMENDED_REFRESH_LEAD_SECONDS
  draftCaptureTriggerMode.value = 'on_attributed_failure'
  draftMaxAttempts.value = RECOMMENDED_CAPTURE_POLICY.maxAttempts
  draftAttemptTimeoutSeconds.value = RECOMMENDED_CAPTURE_POLICY.attemptTimeoutSeconds
  draftJobTimeoutSeconds.value = RECOMMENDED_CAPTURE_POLICY.jobTimeoutSeconds
  draftBackoffSeconds.value = RECOMMENDED_CAPTURE_POLICY.backoffSeconds
  draftMaxBackoffSeconds.value = RECOMMENDED_CAPTURE_POLICY.maxBackoffSeconds
  draftCooldownSeconds.value = RECOMMENDED_CAPTURE_POLICY.cooldownSeconds
}

function openProxyManager() {
  window.open('/proxies', '_blank', 'noopener,noreferrer')
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
    void loadPolicy()
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

function waitingReasonLabel(reason: string) {
  return {
    disabled: '自动捕获已关闭',
    candidate_ready: '候选值已就绪',
    waiting_proxy: '等待选择捕获代理',
    proxy_not_ready: '等待代理测试成功',
    cooldown: '失败冷却中',
    queued: '已进入捕获队列',
    scheduled: '等待提前捕获时间',
    waiting_first_send: '等待当前值首次实际发送',
    waiting_attributed_failure: '等待当前值的归因拒绝',
    waiting_expiry: '等待当前值到期',
    waiting_first_request_after_expiry: '等待到期后的首个业务请求',
    waiting_failure_or_expiry: '等待归因拒绝或到期首请求',
    waiting_normal_observation: '等待普通请求观测',
  }[reason] ?? reason
}

function snapshotPinDraft() {
  return {
    action: pinAction.value,
    value: pinDraftValue.value,
    showPin: showPin.value,
    showPinDraft: showPinDraft.value,
  }
}

function restorePinDraft(draft: ReturnType<typeof snapshotPinDraft>) {
  pinAction.value = draft.action
  pinDraftValue.value = draft.value
  showPin.value = draft.showPin
  showPinDraft.value = draft.showPinDraft
}

async function reloadModelStatePreservingDraft() {
  const requestGeneration = generation
  const requestModelGeneration = modelGeneration
  const draft = snapshotPinDraft()
  await loadModelState()
  if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
    return false
  restorePinDraft(draft)
  return !modelError.value
}

async function handleConflict(accountId: string, model: string, policySaved: boolean) {
  const refreshed = await reloadModelStatePreservingDraft()
  if (!props.open || props.accountId !== accountId || selectedModel.value !== model)
    return
  if (!refreshed) {
    actionError.value = `模型状态已变化，但刷新失败：${modelError.value}。草稿已保留；请先刷新模型状态。`
    return
  }
  actionError.value = policySaved
    ? '账号策略已保存，模型状态变化，草稿已保留，请重新确认。'
    : '模型状态已变化，已载入最新状态并保留草稿；请重新确认后保存。'
}

async function handleCaptureConflict(accountId: string, model: string) {
  const preservedPinDraft = {
    action: pinAction.value,
    value: pinDraftValue.value,
    showPin: showPin.value,
    showPinDraft: showPinDraft.value,
  }
  await loadPolicy()
  await loadModelState()
  if (!props.open || props.accountId !== accountId || selectedModel.value !== model)
    return
  pinAction.value = preservedPinDraft.action
  pinDraftValue.value = preservedPinDraft.value
  showPin.value = preservedPinDraft.showPin
  showPinDraft.value = preservedPinDraft.showPinDraft
  const refreshError = policyError.value || modelError.value
  captureError.value = refreshError
    ? `账号策略或模型状态刷新失败：${refreshError}`
    : captureActive.value
      ? '已有获取任务，已载入最新状态。'
      : '账号策略或模型状态已变化，已载入最新设置并保留模型值草稿；请重新确认后启动任务。'
}

async function savePolicySettings(): Promise<boolean> {
  const current = accountPolicy.value
  if (!current || policySaving.value)
    return false
  if (!policyChanged.value)
    return true
  const requestGeneration = generation
  policySaving.value = true
  actionError.value = ''
  try {
    const result = await updateAccountTurnStatePolicy({
      accountId: current.accountId,
      expectedIdentityRevision: current.identityRevision,
      expectedRevision: current.configRevision,
      lockEnabled: draftLockEnabled.value,
      captureEnabled: draftCaptureEnabled.value,
      reuseWindowSeconds: draftReuseWindowSeconds.value,
      refreshLeadSeconds: draftRefreshLeadSeconds.value,
      captureTriggerMode: draftCaptureTriggerMode.value,
      captureProxyId: draftCaptureProxyId.value || null,
      maxAttempts: draftMaxAttempts.value,
      attemptTimeoutSeconds: draftAttemptTimeoutSeconds.value,
      jobTimeoutSeconds: draftJobTimeoutSeconds.value,
      backoffSeconds: draftBackoffSeconds.value,
      maxBackoffSeconds: draftMaxBackoffSeconds.value,
      cooldownSeconds: draftCooldownSeconds.value,
    })
    if (requestGeneration !== generation)
      return false
    // 只应用账号策略响应，不重新读取模型；pinAction、value 与显隐草稿原样保留。
    applyPolicy(result)
    toast.success('账号 Turn State 策略已保存，适用于该账号的所有模型')
    return true
  }
  catch (error) {
    if (requestGeneration !== generation)
      return false
    if (error instanceof ApiError && error.status === 409) {
      await loadPolicy()
      actionError.value = policyError.value
        ? `账号策略已变化，刷新失败：${policyError.value}`
        : '账号策略已变化，已载入最新设置；请重新确认后保存。'
    }
    else {
      actionError.value = errorMessage(error, '账号 Turn State 策略保存失败')
    }
    return false
  }
  finally {
    if (requestGeneration === generation)
      policySaving.value = false
  }
}

async function saveAllSettings() {
  if (busy.value || pinValidationError.value || (modelChanged.value && modelError.value) || (!policyChanged.value && !modelChanged.value))
    return
  // 开关属于账号策略，模型值是另一个 CAS。统一保存时先提交策略，
  // 避免用户同时替换模型值时只保存了值操作，重开后看到开关仍关闭。
  const policySaved = policyChanged.value
  if (policySaved && !await savePolicySettings())
    return
  if (modelChanged.value)
    await saveModelSettings(policySaved)
}

async function saveModelSettings(policySaved = false) {
  const current = modelState.value
  if (!current || !modelChanged.value || pinValidationError.value || modelError.value || busy.value)
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
      pinAction: pinAction.value,
      ...(pinAction.value === 'replace' ? { value: pinDraftValue.value } : {}),
    })
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    applyModelState(result)
    toast.success('该模型的 Turn State 值操作已保存')
  }
  catch (error) {
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    if (error instanceof ApiError && error.status === 409)
      await handleConflict(current.accountId, current.requestedModel, policySaved)
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
  const policy = accountPolicy.value
  if (!current || !policy || !canStartCapture.value)
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
      expectedPolicyRevision: policy.configRevision,
      expectedIdentityRevision: current.identityRevision,
      expectedEffectiveModel: current.effectiveModel,
    })
    if (requestGeneration !== generation || requestModelGeneration !== modelGeneration)
      return
    captureJob.value = result
    pollingStopped.value = false
    pollDeadline = Date.now() + (policy.capturePolicy.jobTimeoutSeconds + 30) * 1_000
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
          每个账号和实际上游模型独立保存。跨 turn 锁定属于实验行为，统一应用于 Responses HTTP 与 WebSocket，不改变账号调度。
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
        <BaseButton size="sm" class="justify-self-start" @click="reloadModelStatePreservingDraft">
          重试
        </BaseButton>
      </div>
      <template v-else-if="modelState">
        <div v-if="policyLoading" role="status" class="rounded-cp bg-cp-fill-quaternary p-4 text-cp-text-secondary">
          正在读取账号级 Turn State 策略…
        </div>
        <div v-else-if="policyError" role="alert" class="flex flex-wrap items-center justify-between gap-2 rounded-cp bg-cp-error-container p-4 text-cp-error-on-container">
          <span>{{ policyError }}</span>
          <BaseButton size="sm" @click="loadPolicy">
            重试
          </BaseButton>
        </div>
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
              账号身份代次 / 账号策略版本
            </p>
            <p class="mt-1 mb-0 font-mono text-cp-text">
              {{ modelState.identityRevision }} / {{ accountPolicy?.configRevision ?? '—' }}
            </p>
          </div>
        </div>

        <div class="grid gap-2 rounded-cp border border-cp-primary/25 bg-cp-primary-container p-4 text-xs text-cp-primary-on-container">
          <p class="m-0 font-bold">
            推荐流程
          </p>
          <p class="m-0 leading-relaxed">
            以下开关、代理、TTL 与捕获策略按账号保存一次，适用于该账号的所有模型；每个模型的实际密文仍独立保存，绝不跨模型复用。可以先保存开关，缺少代理或代理未测试时只会等待，不影响普通请求。
          </p>
          <p class="m-0 font-bold">
            {{ setupStatus }}
          </p>
        </div>

        <div class="grid gap-4 rounded-cp bg-cp-fill-quaternary p-4 sm:grid-cols-2">
          <div class="flex items-start justify-between gap-3">
            <div>
              <p class="m-0 font-bold text-cp-text">
                账号级自动锁定
              </p>
              <p class="mt-1 mb-0 text-xs leading-relaxed text-cp-text-secondary">
                实验性地跨 turn 注入当前可用值；本地期限到期或明确失效后不再发送旧值。
              </p>
            </div>
            <BaseSwitch v-model="draftLockEnabled" label="启用账号 Turn State 自动锁定" :disabled="busy" active-text="启用" inactive-text="停用" />
          </div>
          <div class="flex items-start justify-between gap-3">
            <div>
              <p class="m-0 font-bold text-cp-text">
                自动捕获
              </p>
              <p class="mt-1 mb-0 text-xs leading-relaxed text-cp-text-secondary">
                没有当前值时，普通 Responses 请求明确返回非 292 字节值会启动一次 bootstrap 捕获；已有值按下方触发模式轮换。也可手动获取。
              </p>
            </div>
            <BaseSwitch v-model="draftCaptureEnabled" label="启用模型 Turn State 自动捕获" :disabled="busy" active-text="启用" inactive-text="停用" @update:model-value="handleCaptureToggle" />
          </div>
        </div>

        <div class="grid gap-4 sm:grid-cols-2">
          <BaseFormItem
            v-if="draftCaptureEnabled"
            label="自动捕获触发方式"
            description="选择何时使用捕获代理。结构化归因拒绝只匹配本次实际发送的账号、模型、generation、candidate 与值。"
          >
            <BaseSelect
              v-model="draftCaptureTriggerMode"
              :options="captureTriggerOptions"
              :disabled="busy"
              aria-label="自动捕获触发方式"
            />
          </BaseFormItem>
          <BaseFormItem control-id="model-turn-state-reuse-window" label="本地最大复用期限" description="默认 7200 秒。当前值到期时会切换到已准备的候选值；没有候选时继续走普通请求。">
            <BaseNumberInput id="model-turn-state-reuse-window" v-model="draftReuseWindowSeconds" aria-describedby="model-turn-state-reuse-window-description" label="本地最大复用期限" :min="1" :max="86400" unit="秒" :disabled="busy" />
            <div class="mt-2 flex flex-wrap gap-1.5" aria-label="复用期限快捷值">
              <BaseButton v-for="hours in [1, 2, 4, 12, 24]" :key="hours" size="sm" variant="ghost" :disabled="busy" @click="draftReuseWindowSeconds = hours * 3600">
                {{ hours }} 小时
              </BaseButton>
            </div>
          </BaseFormItem>
          <BaseFormItem v-if="draftCaptureEnabled && draftCaptureTriggerMode === 'before_expiry_if_used'" control-id="model-turn-state-refresh-lead" label="提前捕获" description="当前值至少实际发送过一次后，默认提前 900 秒排队；大于 TTL 时按 TTL−1 秒执行。">
            <BaseNumberInput id="model-turn-state-refresh-lead" v-model="draftRefreshLeadSeconds" aria-describedby="model-turn-state-refresh-lead-description" label="提前捕获" :min="0" :max="86400" unit="秒" :disabled="busy" />
          </BaseFormItem>
          <BaseFormItem
            label="捕获代理"
            description="选择已管理且在最近 24 小时测试成功的代理。DataImpulse 等服务可先把 rotating URL 添加为代理并完成测试。"
          >
            <BaseSelect
              v-model="draftCaptureProxyId"
              :options="proxyOptions"
              :disabled="proxiesLoading || busy"
              :placeholder="proxiesLoading ? '加载代理中…' : '选择捕获代理'"
              empty-text="还没有已管理代理"
              aria-label="捕获代理"
            />
            <div class="mt-2 flex flex-wrap gap-2">
              <BaseButton size="sm" variant="ghost" :disabled="proxiesLoading || busy" @click="loadProxies">
                刷新代理状态
              </BaseButton>
              <BaseButton size="sm" variant="secondary" :disabled="busy" @click="openProxyManager">
                打开代理管理
              </BaseButton>
            </div>
            <p v-if="captureProxyValidationError" role="status" class="mt-2 mb-0 text-xs text-cp-warning-text">
              {{ captureProxyValidationError }}
            </p>
            <p v-if="!proxiesLoading && !proxiesError && proxies.length === 0" class="mt-2 mb-0 text-xs text-cp-text-quaternary">
              还没有已管理代理。代理管理会在新标签页打开；添加并测试后，回到这里点击“刷新代理状态”。
            </p>
            <div v-if="proxiesError" role="alert" class="mt-2 flex flex-wrap items-center gap-2 text-xs text-cp-error-text">
              <span>{{ proxiesError }}</span>
              <BaseButton size="sm" variant="ghost" @click="loadProxies">
                重试
              </BaseButton>
            </div>
          </BaseFormItem>
        </div>

        <div class="flex flex-wrap items-center justify-between gap-2 rounded-cp bg-cp-bg-container px-4 py-3">
          <p class="m-0 text-xs text-cp-text-secondary">
            保存后立即应用到此账号的所有模型；缺少可用代理时自动捕获保持等待。
          </p>
          <BaseButton
            variant="primary"
            :loading="policySaving"
            :disabled="!policyChanged || busy"
            @click="savePolicySettings"
          >
            保存账号策略
          </BaseButton>
        </div>

        <section v-if="modelState.legacyOverride.configured && modelState.legacyOverride.value" class="grid min-w-0 gap-3 rounded-cp bg-cp-fill-quaternary p-4" aria-label="待导入的旧账号值">
          <div>
            <h4 class="m-0 font-bold text-cp-text">
              待导入的旧账号值
            </h4>
            <p class="mt-1 mb-0 text-xs text-cp-text-secondary">
              旧账号覆盖已退出运行时优先级。确认模型后可把它显式导入当前使用值；不会自动复制到其他模型。
            </p>
          </div>
          <div class="flex min-w-0 flex-wrap items-center gap-2">
            <BaseInput :model-value="modelState.legacyOverride.value" :type="showLegacy ? 'text' : 'password'" readonly aria-label="旧账号 Turn State 值" class="min-w-40 flex-1" />
            <BaseButton size="sm" @click="showLegacy = !showLegacy">
              <template #icon>
                <EyeOff v-if="showLegacy" :size="14" /><Eye v-else :size="14" />
              </template>
              {{ showLegacy ? '隐藏' : '显示' }}
            </BaseButton>
            <BaseButton size="sm" @click="copyText(modelState.legacyOverride.value!, { successText: '旧账号值已复制' })">
              <template #icon>
                <Copy :size="14" />
              </template>
              复制
            </BaseButton>
            <BaseButton size="sm" :variant="pinAction === 'importLegacy' ? 'primary' : 'secondary'" :disabled="busy" @click="setPinAction('importLegacy')">
              {{ pinAction === 'importLegacy' ? '等待保存导入' : '导入当前模型' }}
            </BaseButton>
          </div>
        </section>

        <section class="grid min-w-0 gap-3 rounded-cp bg-cp-bg-container p-4 shadow-cp-tertiary" aria-label="模型锁定值">
          <div class="flex flex-wrap items-center justify-between gap-2">
            <div>
              <h4 class="m-0 font-bold text-cp-text">
                当前使用值
              </h4>
              <p class="mt-1 mb-0 text-xs text-cp-text-secondary">
                手动值支持 1–16384 字节可打印 ASCII；普通观测与自动捕获仍只接纳 292 字节可打印 ASCII。Fernet 字段只解析公开外层结构，不验证密文或 HMAC。
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
                  {{ modelState.pin.source === 'manual' ? '手动保存' : modelState.pin.source === 'capture' ? '住宅代理捕获' : '普通请求观测' }}
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
                  {{ modelState.pin.compatibleTransports.join(' / ') }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  实际发送次数
                </dt><dd class="m-0 text-cp-text">
                  {{ modelState.pin.sentCount }} 次
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  最近实际发送
                </dt><dd class="m-0 text-cp-text">
                  {{ modelState.pin.lastSentAt ? formatDateTime(modelState.pin.lastSentAt) : '尚未发送' }}
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
            当前模型还没有保存值。锁定开关仍可保存为“获取后自动锁定”；系统会先从普通 Responses 请求观测，必要时再按自动捕获设置使用代理。
          </p>

          <div class="flex flex-wrap gap-2">
            <BaseButton size="sm" :variant="pinAction === 'replace' ? 'primary' : 'secondary'" :disabled="busy" @click="setPinAction('replace')">
              手动替换
            </BaseButton>
            <BaseButton size="sm" :variant="pinAction === 'invalidate' ? 'primary' : 'secondary'" :disabled="!modelState.pin || busy" @click="setPinAction('invalidate')">
              标记失效
            </BaseButton>
            <BaseButton size="sm" :variant="pinAction === 'clear' ? 'destructive' : 'secondary'" :disabled="!modelState.pin || busy" @click="setPinAction('clear')">
              清除保存值
            </BaseButton>
            <BaseButton v-if="pinAction !== 'keep'" size="sm" variant="ghost" :disabled="busy" @click="setPinAction('keep')">
              取消值操作
            </BaseButton>
          </div>
          <BaseFormItem v-if="pinAction === 'replace'" label="新的模型 Turn State 值" :error="pinValidationError">
            <div class="flex min-w-0 flex-wrap items-center gap-2">
              <BaseInput v-model="pinDraftValue" :type="showPinDraft ? 'text' : 'password'" autocomplete="off" placeholder="输入 1–16384 字节的可打印 ASCII" :disabled="busy" class="min-w-40 flex-1" />
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
            <p class="mt-2 mb-0 text-xs" :class="pinDraftBytes >= 1 && pinDraftBytes <= 16384 ? 'text-cp-info-text' : 'text-cp-text-quaternary'">
              当前 {{ pinDraftBytes }} / 16384 UTF-8 字节
            </p>
          </BaseFormItem>
          <p v-else-if="pinAction === 'importLegacy'" role="status" class="m-0 text-xs text-cp-info-text">
            保存后会把旧账号覆盖导入当前所选模型，立即成为同一个当前使用值；旧值不再作为运行时 fallback。
          </p>
          <p v-else-if="pinAction === 'invalidate'" role="status" class="m-0 text-xs text-cp-warning-text">
            保存后，这个值会保留用于审计，但不再注入；若锁定开关仍开启，会回到待获取状态。
          </p>
          <p v-else-if="pinAction === 'clear'" role="status" class="m-0 text-xs text-cp-error-text">
            保存后会删除这个模型的当前值；若锁定开关仍开启，会回到待获取状态。
          </p>
          <p v-if="modelState.pin?.status === 'aged'" role="status" class="m-0 text-xs text-cp-warning-text">
            这个值已停止注入。下一次普通 HTTP 请求返回 292 字节可打印 ASCII 值时会直接采用；明确返回非 292 字节值时才会触发已配置的住宅代理捕获。也可以手动获取或替换。
          </p>
        </section>

        <section class="grid min-w-0 gap-3 rounded-cp bg-cp-bg-container p-4 shadow-cp-tertiary" aria-label="待切换值">
          <div>
            <h4 class="m-0 font-bold text-cp-text">
              待切换值
            </h4>
            <p class="mt-1 mb-0 text-xs text-cp-text-secondary">
              自动捕获会先把新值放在这里；当前值到期或被明确判定失效时再切换，不会重置候选值自己的 TTL。
            </p>
          </div>
          <template v-if="modelState.candidate">
            <div class="flex min-w-0 flex-wrap items-center gap-2">
              <BaseInput :model-value="modelState.candidate.value" type="password" readonly aria-label="待切换 Turn State 值" class="min-w-40 flex-1" />
              <BaseButton size="sm" @click="copyText(modelState.candidate!.value, { successText: '待切换值已复制' })">
                <template #icon>
                  <Copy :size="14" />
                </template>
                复制
              </BaseButton>
            </div>
            <dl class="m-0 grid gap-x-4 gap-y-2 text-xs sm:grid-cols-3">
              <div>
                <dt class="text-cp-text-quaternary">
                  捕获时间
                </dt><dd class="m-0 text-cp-text">
                  {{ formatDateTime(modelState.candidate.capturedAt) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  候选截止
                </dt><dd class="m-0 text-cp-text">
                  {{ formatDateTime(modelState.candidate.reuseDeadline) }}
                </dd>
              </div>
              <div>
                <dt class="text-cp-text-quaternary">
                  预计切换
                </dt><dd class="m-0 text-cp-text">
                  {{ modelState.nextActivationAt ? formatDateTime(modelState.nextActivationAt) : '等待当前值到期' }}
                </dd>
              </div>
            </dl>
          </template>
          <p v-else class="m-0 text-cp-text-secondary">
            暂无待切换值。{{ modelState.nextCaptureAt ? `预计 ${formatDateTime(modelState.nextCaptureAt)} 开始排队` : '系统会按账号策略准备下一份值。' }}
          </p>
          <p v-if="modelState.waitingReason" class="m-0 text-xs text-cp-text-quaternary">
            调度状态：{{ waitingReasonLabel(modelState.waitingReason) }}<template v-if="modelState.captureNotBefore">
              · 不早于 {{ formatDateTime(modelState.captureNotBefore) }}
            </template>
          </p>
        </section>

        <details class="group rounded-cp bg-cp-fill-quaternary">
          <summary class="cursor-pointer rounded-cp px-4 py-3 font-bold text-cp-text hover:bg-cp-bg-text-hover focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-cp-primary">
            捕获策略高级设置（默认通常无需修改）
          </summary>
          <div class="grid gap-4 px-4 pb-4 sm:grid-cols-2 lg:grid-cols-3">
            <div class="flex items-end sm:col-span-2 lg:col-span-3">
              <BaseButton size="sm" variant="ghost" :disabled="busy" @click="applyRecommendedDefaults">
                恢复推荐默认值
              </BaseButton>
            </div>
            <BaseFormItem control-id="model-turn-state-max-attempts" label="最大尝试次数" description="1–10 次">
              <BaseNumberInput id="model-turn-state-max-attempts" v-model="draftMaxAttempts" aria-describedby="model-turn-state-max-attempts-description" label="最大尝试次数" :min="1" :max="10" unit="次" :disabled="busy" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-attempt-timeout" label="单次尝试超时" description="1–60 秒；运行时不会超过任务剩余时间">
              <BaseNumberInput id="model-turn-state-attempt-timeout" v-model="draftAttemptTimeoutSeconds" aria-describedby="model-turn-state-attempt-timeout-description" label="单次尝试超时" :min="1" :max="60" unit="秒" :disabled="busy" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-job-timeout" label="任务总超时" description="1–300 秒">
              <BaseNumberInput id="model-turn-state-job-timeout" v-model="draftJobTimeoutSeconds" aria-describedby="model-turn-state-job-timeout-description" label="任务总超时" :min="1" :max="300" unit="秒" :disabled="busy" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-backoff" label="首次退避" description="0–60 秒">
              <BaseNumberInput id="model-turn-state-backoff" v-model="draftBackoffSeconds" aria-describedby="model-turn-state-backoff-description" label="首次退避" :min="0" :max="60" unit="秒" :disabled="busy" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-max-backoff" label="最大退避" description="0–60 秒；小于首次退避时按此上限截断">
              <BaseNumberInput id="model-turn-state-max-backoff" v-model="draftMaxBackoffSeconds" aria-describedby="model-turn-state-max-backoff-description" label="最大退避" :min="0" :max="60" unit="秒" :disabled="busy" />
            </BaseFormItem>
            <BaseFormItem control-id="model-turn-state-cooldown" label="失败冷却" description="0–86400 秒">
              <BaseNumberInput id="model-turn-state-cooldown" v-model="draftCooldownSeconds" aria-describedby="model-turn-state-cooldown-description" label="失败冷却" :min="0" :max="86400" unit="秒" :disabled="busy" />
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
          <p v-if="accountPolicy?.captureReadiness !== 'ready'" class="m-0 text-xs text-cp-warning-text">
            手动获取需要账号策略中已选择且最近 24 小时测试成功的代理；自动流程会保持等待，不影响普通请求。
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

        <p v-if="actionError" role="alert" class="m-0 text-cp-error-text">
          {{ actionError }}
        </p>
        <div class="flex flex-wrap items-center justify-between gap-2">
          <p class="m-0 text-xs text-cp-text-secondary">
            这里会同时保存账号开关与当前模型的值操作。
          </p>
          <BaseButton
            variant="primary"
            :loading="busy"
            :disabled="(!policyChanged && !modelChanged) || !!pinValidationError || (modelChanged && !!modelError) || busy"
            @click="saveAllSettings"
          >
            保存所有更改
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
