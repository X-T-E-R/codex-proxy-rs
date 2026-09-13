import type { rotationOptions } from '../constants'
import { computed, reactive, ref, shallowRef } from 'vue'

import { getSettings, updateSettings } from '@/api'
import { toast } from '@/components/base/BaseToast'
import { errorMessage } from '@/utils/async'

type RotationStrategy = (typeof rotationOptions)[number]['value']

export function useSettingsForm() {
  const loading = shallowRef(true)
  const saving = shallowRef(false)
  const error = shallowRef('')
  const mappings = ref<Array<{ requestedModel: string, upstreamModel: string }>>([])
  const form = reactive({
    refreshMarginSeconds: null as number | null,
    refreshConcurrency: null as number | null,
    maxConcurrentPerAccount: null as number | null,
    requestIntervalMs: null as number | null,
    rotationStrategy: '' as RotationStrategy | '',
    minCodexDesktopVersion: '',
    minCodexCliVersion: '',
    usageRetentionDays: 31,
    opsEventRetentionDays: 30,
    auditRetentionDays: 90,
    wsPoolEnabled: true,
    wsPoolMaxAgeMs: null as number | null,
    wsPoolMaxConnecting: null as number | null,
    wsPoolStreamIdleTimeoutMs: null as number | null,
    wsPoolFastPathBudgetMs: null as number | null,
    overloadCooldownEnabled: false,
    openaiUserAgent: '',
    openaiRequestBodyOverrideEnabled: true,
    openaiRequestTimezone: 'America/Los_Angeles',
    openaiSearchCountry: 'US',
    overloadCooldownThreshold: null as number | null,
    overloadCooldownSeconds: null as number | null,
    cyberSessionBlockEnabled: false,
    cyberSessionBlockTtlSeconds: null as number | null,
  })

  function numericModel(key: 'refreshMarginSeconds' | 'refreshConcurrency' | 'maxConcurrentPerAccount' | 'requestIntervalMs' | 'wsPoolMaxAgeMs' | 'wsPoolMaxConnecting' | 'wsPoolStreamIdleTimeoutMs' | 'wsPoolFastPathBudgetMs' | 'overloadCooldownThreshold' | 'overloadCooldownSeconds' | 'cyberSessionBlockTtlSeconds') {
    return computed({
      get: () => (form[key] === null ? '' : String(form[key])),
      set: (value: string) => {
        if (!value.trim()) {
          form[key] = null
          return
        }
        const parsed = Number(value)
        form[key] = Number.isFinite(parsed) ? parsed : null
      },
    })
  }

  const refreshMarginSecondsValue = numericModel('refreshMarginSeconds')
  const refreshConcurrencyValue = numericModel('refreshConcurrency')
  const maxConcurrentPerAccountValue = numericModel('maxConcurrentPerAccount')
  const requestIntervalMsValue = numericModel('requestIntervalMs')
  const wsPoolMaxAgeMsValue = numericModel('wsPoolMaxAgeMs')
  const wsPoolMaxConnectingValue = numericModel('wsPoolMaxConnecting')
  const wsPoolStreamIdleTimeoutMsValue = numericModel('wsPoolStreamIdleTimeoutMs')
  const wsPoolFastPathBudgetMsValue = numericModel('wsPoolFastPathBudgetMs')
  const overloadCooldownThresholdValue = numericModel('overloadCooldownThreshold')
  const overloadCooldownSecondsValue = numericModel('overloadCooldownSeconds')
  const cyberSessionBlockTtlSecondsValue = numericModel('cyberSessionBlockTtlSeconds')
  const cyberSessionBlockTtlError = computed(() => positiveIntegerError(form.cyberSessionBlockTtlSeconds, 4_294_967_295))
  const overloadCooldownErrors = computed(() => ({
    threshold: positiveIntegerError(form.overloadCooldownThreshold, 4_294_967_295),
    seconds: positiveIntegerError(form.overloadCooldownSeconds, 4_294_967_295),
  }))
  const wsPoolErrors = computed(() => ({
    maxAge: positiveIntegerError(form.wsPoolMaxAgeMs),
    maxConnecting: positiveIntegerError(form.wsPoolMaxConnecting, 4_294_967_295),
    streamIdleTimeout: positiveIntegerError(form.wsPoolStreamIdleTimeoutMs),
    fastPathBudget: positiveIntegerError(form.wsPoolFastPathBudgetMs),
  }))
  const minCodexDesktopVersionError = computed(() => versionError(form.minCodexDesktopVersion))
  const minCodexCliVersionError = computed(() => versionError(form.minCodexCliVersion))
  const openaiUserAgentError = computed(() => {
    const value = form.openaiUserAgent.trim()
    return value && (value.length > 512 || !/^[\x20-\x7E]+$/.test(value))
      ? '请输入最多 512 个可打印 ASCII 字符，不含换行'
      : ''
  })
  const requestLocaleErrors = computed(() => ({
    timezone: loading.value ? '' : timezoneError(form.openaiRequestTimezone),
    country: loading.value || /^[a-z]{2}$/i.test(form.openaiSearchCountry.trim())
      ? ''
      : '请输入两位字母国家代码，例如 US',
  }))

  function timezoneError(value: string): string {
    const normalized = value.trim()
    if (!normalized || normalized.length > 128)
      return '请输入 IANA 时区，例如 America/Los_Angeles'
    try {
      new Intl.DateTimeFormat('en-US', { timeZone: normalized }).format()
      return ''
    }
    catch {
      return '请输入有效的 IANA 时区，例如 America/Los_Angeles'
    }
  }

  function positiveIntegerError(value: number | null, max = Number.MAX_SAFE_INTEGER): string {
    if (loading.value)
      return ''
    if (value === null || !Number.isSafeInteger(value) || value < 1)
      return '请输入大于 0 的整数'
    return value > max ? `最大值为 ${max}` : ''
  }

  function versionError(value: string): string {
    const normalized = value.trim()
    return normalized && !isSemver(normalized) ? '请输入标准 SemVer，例如 0.152.0' : ''
  }

  function applySettings(data: Awaited<ReturnType<typeof getSettings>>) {
    form.refreshMarginSeconds = data.refreshMarginSeconds
    form.refreshConcurrency = data.refreshConcurrency
    form.maxConcurrentPerAccount = data.maxConcurrentPerAccount
    form.requestIntervalMs = data.requestIntervalMs
    form.rotationStrategy = data.rotationStrategy
    form.minCodexDesktopVersion = data.minCodexDesktopVersion ?? ''
    form.minCodexCliVersion = data.minCodexCliVersion ?? ''
    form.usageRetentionDays = data.usageRetentionDays
    form.opsEventRetentionDays = data.opsEventRetentionDays
    form.auditRetentionDays = data.auditRetentionDays
    form.wsPoolEnabled = data.wsPoolEnabled
    form.wsPoolMaxAgeMs = data.wsPoolMaxAgeMs
    form.wsPoolMaxConnecting = data.wsPoolMaxConnecting
    form.wsPoolStreamIdleTimeoutMs = data.wsPoolStreamIdleTimeoutMs
    form.wsPoolFastPathBudgetMs = data.wsPoolFastPathBudgetMs
    form.overloadCooldownEnabled = data.overloadCooldownEnabled
    form.overloadCooldownThreshold = data.overloadCooldownThreshold
    form.overloadCooldownSeconds = data.overloadCooldownSeconds
    form.cyberSessionBlockEnabled = data.cyberSessionBlockEnabled
    form.cyberSessionBlockTtlSeconds = data.cyberSessionBlockTtlSeconds
    form.openaiUserAgent = data.openaiUserAgent ?? ''
    form.openaiRequestBodyOverrideEnabled = data.openaiRequestBodyOverrideEnabled
    form.openaiRequestTimezone = data.openaiRequestTimezone
    form.openaiSearchCountry = data.openaiSearchCountry
    mappings.value = Object.entries(data.modelMappings || {}).map(([requestedModel, upstreamModel]) => ({
      requestedModel,
      upstreamModel: String(upstreamModel),
    }))
  }

  async function loadSettings() {
    loading.value = true
    error.value = ''
    try {
      applySettings(await getSettings())
    }
    catch (cause: unknown) {
      error.value = errorMessage(cause, '设置加载失败')
      toast.error(error.value)
    }
    finally {
      loading.value = false
    }
  }

  function addMapping() {
    mappings.value = [...mappings.value, { requestedModel: '', upstreamModel: '' }]
  }

  function updateMapping(index: number, key: 'requestedModel' | 'upstreamModel', value: string) {
    const rows = [...mappings.value]
    if (!rows[index])
      return
    rows[index] = { ...rows[index], [key]: value }
    mappings.value = rows
  }

  function removeMapping(index: number) {
    const rows = [...mappings.value]
    rows.splice(index, 1)
    mappings.value = rows
  }

  function mappingPayload() {
    const entries: Record<string, string> = {}
    for (const row of mappings.value) {
      const requested = row.requestedModel.trim()
      const upstream = row.upstreamModel.trim()
      if (!requested || !upstream)
        throw new Error('请完整填写模型映射')
      if (entries[requested])
        throw new Error(`存在重复的客户端模型：${requested}`)
      entries[requested] = upstream
    }
    return entries
  }

  async function saveSettings() {
    if (saving.value || loading.value)
      return
    const { refreshMarginSeconds, refreshConcurrency, maxConcurrentPerAccount, requestIntervalMs, rotationStrategy, wsPoolMaxAgeMs, wsPoolMaxConnecting, wsPoolStreamIdleTimeoutMs, wsPoolFastPathBudgetMs, overloadCooldownThreshold, overloadCooldownSeconds, cyberSessionBlockTtlSeconds } = form
    if (refreshMarginSeconds === null || refreshConcurrency === null || maxConcurrentPerAccount === null || requestIntervalMs === null || !rotationStrategy) {
      toast.warning('请完整填写运行参数和调度策略')
      return
    }
    if (wsPoolMaxAgeMs === null || wsPoolMaxConnecting === null || wsPoolStreamIdleTimeoutMs === null || wsPoolFastPathBudgetMs === null) {
      toast.warning('请完整填写 WebSocket 连接池参数')
      return
    }
    if (Object.values(wsPoolErrors.value).some(Boolean)) {
      toast.warning('请修正 WebSocket 连接池参数')
      return
    }
    if (overloadCooldownThreshold === null || overloadCooldownSeconds === null || Object.values(overloadCooldownErrors.value).some(Boolean)) {
      toast.warning('请修正过载冷号参数')
      return
    }
    if (minCodexDesktopVersionError.value || minCodexCliVersionError.value) {
      toast.warning('请修正客户端最低版本格式')
      return
    }
    if (cyberSessionBlockTtlSeconds === null || cyberSessionBlockTtlError.value) {
      toast.warning('请修正 cyber 会话屏蔽时长')
      return
    }
    if (openaiUserAgentError.value) {
      toast.warning('请修正上游 User-Agent')
      return
    }
    if (Object.values(requestLocaleErrors.value).some(Boolean)) {
      toast.warning('请修正请求正文的时区与国家代码')
      return
    }
    try {
      saving.value = true
      const result = await updateSettings({
        modelMappings: mappingPayload(),
        refreshMarginSeconds,
        refreshConcurrency,
        maxConcurrentPerAccount,
        requestIntervalMs,
        rotationStrategy,
        minCodexDesktopVersion: form.minCodexDesktopVersion.trim() || null,
        minCodexCliVersion: form.minCodexCliVersion.trim() || null,
        usageRetentionDays: form.usageRetentionDays,
        opsEventRetentionDays: form.opsEventRetentionDays,
        auditRetentionDays: form.auditRetentionDays,
        wsPoolEnabled: form.wsPoolEnabled,
        wsPoolMaxAgeMs,
        wsPoolMaxConnecting,
        wsPoolStreamIdleTimeoutMs,
        wsPoolFastPathBudgetMs,
        overloadCooldownEnabled: form.overloadCooldownEnabled,
        overloadCooldownThreshold,
        overloadCooldownSeconds,
        cyberSessionBlockEnabled: form.cyberSessionBlockEnabled,
        cyberSessionBlockTtlSeconds,
        openaiUserAgent: form.openaiUserAgent.trim() || null,
        openaiRequestBodyOverrideEnabled: form.openaiRequestBodyOverrideEnabled,
        openaiRequestTimezone: form.openaiRequestTimezone.trim(),
        openaiSearchCountry: form.openaiSearchCountry.trim().toUpperCase(),
      })
      applySettings(result)
      toast.success('设置已保存')
    }
    catch (cause: unknown) {
      error.value = errorMessage(cause, '保存失败')
      toast.error(error.value)
      await loadSettings()
    }
    finally {
      saving.value = false
    }
  }

  return {
    loading,
    saving,
    error,
    form,
    mappings,
    addMapping,
    updateMapping,
    removeMapping,
    refreshMarginSecondsValue,
    refreshConcurrencyValue,
    maxConcurrentPerAccountValue,
    requestIntervalMsValue,
    wsPoolMaxAgeMsValue,
    wsPoolMaxConnectingValue,
    wsPoolStreamIdleTimeoutMsValue,
    wsPoolFastPathBudgetMsValue,
    wsPoolErrors,
    overloadCooldownThresholdValue,
    overloadCooldownSecondsValue,
    overloadCooldownErrors,
    cyberSessionBlockTtlSecondsValue,
    cyberSessionBlockTtlError,
    openaiUserAgentError,
    requestLocaleErrors,
    minCodexDesktopVersionError,
    minCodexCliVersionError,
    saveSettings,
    loadSettings,
  }
}

function isSemver(value: string): boolean {
  if (value.length > 64 || value.startsWith('v'))
    return false

  const buildParts = value.split('+')
  if (buildParts.length > 2)
    return false
  const [versionAndPrerelease = '', build] = buildParts
  if (build !== undefined && !validIdentifiers(build, false))
    return false

  const prereleaseSeparator = versionAndPrerelease.indexOf('-')
  const core = prereleaseSeparator < 0
    ? versionAndPrerelease
    : versionAndPrerelease.slice(0, prereleaseSeparator)
  const prerelease = prereleaseSeparator < 0
    ? undefined
    : versionAndPrerelease.slice(prereleaseSeparator + 1)
  if (prerelease !== undefined && !validIdentifiers(prerelease, true))
    return false

  const coreParts = core.split('.')
  return coreParts.length === 3 && coreParts.every(validCoreNumericIdentifier)
}

function validIdentifiers(value: string, rejectNumericLeadingZeros: boolean): boolean {
  return Boolean(value) && value.split('.').every((identifier) => {
    if (!identifier || !/^[\da-z-]+$/i.test(identifier))
      return false
    return !rejectNumericLeadingZeros || !/^\d+$/.test(identifier) || validNumericIdentifier(identifier)
  })
}

function validNumericIdentifier(value: string): boolean {
  return /^(?:0|[1-9]\d*)$/.test(value)
}

function validCoreNumericIdentifier(value: string): boolean {
  return validNumericIdentifier(value) && BigInt(value) <= 18_446_744_073_709_551_615n
}
