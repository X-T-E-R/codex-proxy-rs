import type { AccountQuotaPolicy, QuotaPolicy, QuotaPolicyAction } from '@/api'

export interface QuotaPolicyForm {
  primaryAction: QuotaPolicyAction
  primaryThreshold: string
  secondaryAction: QuotaPolicyAction
  secondaryThreshold: string
  maxAttemptsPer24h: string
  cooldownSeconds: string
}

export const quotaActionOptions = [
  { value: 'off', label: '关闭' },
  { value: 'stop', label: '停止新分配' },
  { value: 'reset_then_stop', label: '尝试重置，否则停止' },
]

export const quotaModeOptions = [
  { value: 'inherit', label: '继承全局' },
  { value: 'disabled', label: '禁用本账号策略' },
  { value: 'custom', label: '账号自定义' },
]

export function quotaPolicyForm(policy?: QuotaPolicy): QuotaPolicyForm {
  return {
    primaryAction: policy?.primary.action ?? 'off',
    primaryThreshold: String(policy?.primary.thresholdPercent ?? 100),
    secondaryAction: policy?.secondary.action ?? 'off',
    secondaryThreshold: String(policy?.secondary.thresholdPercent ?? 100),
    maxAttemptsPer24h: String(policy?.autoReset.maxAttemptsPer24h ?? 1),
    cooldownSeconds: String(policy?.autoReset.cooldownSeconds ?? 86400),
  }
}

export function quotaPolicyError(form: QuotaPolicyForm): string {
  for (const value of [form.primaryThreshold, form.secondaryThreshold]) {
    if (!value.trim() || !Number.isInteger(Number(value)) || Number(value) < 1 || Number(value) > 100)
      return '已用比例阈值需为 1% 到 100% 之间的整数'
  }
  const attempts = Number(form.maxAttemptsPer24h)
  if (!Number.isInteger(attempts) || attempts < 1 || attempts > 10)
    return '滚动 24 小时自动发送上限需为 1 到 10 次'
  const seconds = Number(form.cooldownSeconds)
  if (!Number.isInteger(seconds) || seconds < 3600 || seconds > 86400)
    return '自动发送最小间隔需为 3600 到 86400 秒'
  return ''
}

export function policyFromForm(form: QuotaPolicyForm): QuotaPolicy {
  return {
    primary: { action: form.primaryAction, thresholdPercent: Number(form.primaryThreshold) },
    secondary: { action: form.secondaryAction, thresholdPercent: Number(form.secondaryThreshold) },
    autoReset: { maxAttemptsPer24h: Number(form.maxAttemptsPer24h), cooldownSeconds: Number(form.cooldownSeconds) },
  }
}

export function hasAutoReset(policy: QuotaPolicy) {
  return policy.primary.action === 'reset_then_stop' || policy.secondary.action === 'reset_then_stop'
}

export function quotaPolicySource(source: AccountQuotaPolicy['source']) {
  return { global: '全局默认', disabled: '本账号已禁用策略', account: '账号自定义' }[source]
}

export function quotaPolicySummary(policy: QuotaPolicy) {
  return (['primary', 'secondary'] as const).map((key) => {
    const window = policy[key]
    const label = key === 'primary' ? '主窗口' : '次窗口'
    if (window.action === 'off')
      return `${label}关闭`
    return `${label} ≥ ${window.thresholdPercent}% ${window.action === 'stop' ? '停止新分配' : '尝试重置，否则停止'}`
  }).join(' · ')
}

export function quotaPolicyReason(reason: string | null) {
  switch (reason) {
    case 'off': return '额度策略未启用'
    case 'ready': return '额度策略未暂停新分配'
    case 'threshold_primary': return '主窗口达到已用比例阈值'
    case 'threshold_secondary': return '次窗口达到已用比例阈值'
    case 'quota_unconfirmed': return '额度尚未确认，请刷新额度'
    case 'reset_pending': return '重置结果待确认，确认前不会消耗下一张卡'
    case 'reset_failed': return '自动重置未成功，继续暂停新分配'
    case 'reset_readback_unconfirmed': return '重置已成功，最新额度未确认，尚未恢复新分配'
    case 'auto_budget_exhausted': return '已达到滚动 24 小时自动发送上限'
    case 'auto_cooldown': return '尚未达到自动发送最小间隔'
    default: return '策略状态待确认，请刷新读取'
  }
}
