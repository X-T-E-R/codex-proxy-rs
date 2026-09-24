import request from '../request'

export type RotationStrategy = 'smart' | 'quota_reset_priority' | 'round_robin' | 'sticky'

// 模型级请求策略：按客户端请求的 public 模型名精确匹配。
export type ReasoningEffortMode = 'locked' | 'min' | 'max'
export type ReasoningEffortValue
  = 'none' | 'minimal' | 'low' | 'medium' | 'high' | 'xhigh' | 'max'
export type ServiceTierRule = 'lock_fast' | 'lock_never_fast'

export interface ReasoningEffortRule {
  mode: ReasoningEffortMode
  value: ReasoningEffortValue
}

export interface ModelRequestPolicy {
  reasoningEffort: ReasoningEffortRule | null
  serviceTier: ServiceTierRule | null
}

export interface RuntimeSettings {
  modelMappings: Record<string, string>
  modelPolicies: Record<string, ModelRequestPolicy>
  refreshMarginSeconds: number
  refreshConcurrency: number
  maxConcurrentPerAccount: number
  requestIntervalMs: number
  rotationStrategy: RotationStrategy
  minCodexDesktopVersion: string | null
  minCodexCliVersion: string | null
  usageRetentionDays: number
  opsEventRetentionDays: number
  auditRetentionDays: number
  wsPoolEnabled: boolean
  wsPoolMaxAgeMs: number
  wsPoolMaxConnecting: number
  wsPoolStreamIdleTimeoutMs: number
  wsPoolFastPathBudgetMs: number
  overloadCooldownEnabled: boolean
  overloadCooldownThreshold: number
  overloadCooldownSeconds: number
  cyberSessionBlockEnabled: boolean
  cyberSessionBlockTtlSeconds: number
  openaiUserAgent: string | null
  openaiRequestBodyOverrideEnabled: boolean
  openaiRequestTimezone: string
  openaiSearchCountry: string
  updatedAt: string
}

export type ClientArchitecture = 'x64' | 'arm64'
export type ClientDownloadSource = 'microsoft_store' | 'official_openai'

export interface ClientDownloadPackage {
  architecture: ClientArchitecture
  source: ClientDownloadSource
  version: string | null
  fileName: string
  sizeBytes: number | null
  downloadUrl: string
  expiresAt: string | null
}

export interface CodexDesktopWindowsDownloads {
  resolvedAt: string
  cached: boolean
  warning: string | null
  packages: ClientDownloadPackage[]
}

export interface AdminApiKeyStatus {
  exists: boolean
}

export interface RegeneratedAdminApiKey {
  key: string
}

export interface DeletedAdminApiKey {
  message: string
}

export function getSettings() {
  return request<RuntimeSettings>({
    url: '/api/admin/settings',
    method: 'GET',
  })
}

type UpdateSettingsParam = Omit<RuntimeSettings, 'updatedAt' | 'modelPolicies'> & {
  // 部分更新语义：省略时保留当前值；提供时整体替换。
  modelPolicies?: Record<string, ModelRequestPolicy>
}

export function updateSettings(data: UpdateSettingsParam) {
  return request<RuntimeSettings>({
    url: '/api/admin/settings/update',
    method: 'POST',
    data,
  })
}

export function getAdminApiKeyStatus() {
  return request<AdminApiKeyStatus>({
    url: '/api/admin/settings/admin-api-key',
    method: 'GET',
  })
}

export function regenerateAdminApiKey() {
  return request<RegeneratedAdminApiKey>({
    url: '/api/admin/settings/admin-api-key/regenerate',
    method: 'POST',
  })
}

export function deleteAdminApiKey() {
  return request<DeletedAdminApiKey>({
    url: '/api/admin/settings/admin-api-key/delete',
    method: 'POST',
  })
}

export function getCodexDesktopWindowsDownloads(refresh = false) {
  return request<CodexDesktopWindowsDownloads>({
    url: '/api/admin/settings/client-downloads/codex-desktop/windows',
    method: 'GET',
    params: refresh ? { refresh: true } : undefined,
  })
}
