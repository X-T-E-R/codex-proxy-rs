import type { QuotaPolicy, QuotaPolicyAction } from '@/api'

export interface QuotaPolicyForm {
  primaryAction: QuotaPolicyAction
  primaryThreshold: string
  secondaryAction: QuotaPolicyAction
  secondaryThreshold: string
  maxAttemptsPer24h: string
  cooldownSeconds: string
}

export type QuotaPolicyErrorKey = 'threshold' | 'attempts' | 'cooldown'

// 阈值来自 GET 回填；动作关闭时保留原阈值，避免保存后把 99% 覆盖成默认值
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

export function policyFromForm(form: QuotaPolicyForm): QuotaPolicy {
  return {
    primary: { action: form.primaryAction, thresholdPercent: Number(form.primaryThreshold) },
    secondary: { action: form.secondaryAction, thresholdPercent: Number(form.secondaryThreshold) },
    autoReset: { maxAttemptsPer24h: Number(form.maxAttemptsPer24h), cooldownSeconds: Number(form.cooldownSeconds) },
  }
}

export function hasAutoReset(policy: QuotaPolicy): boolean {
  return policy.primary.action === 'reset_then_stop' || policy.secondary.action === 'reset_then_stop'
}

export function quotaPolicyErrorKey(form: QuotaPolicyForm): QuotaPolicyErrorKey | null {
  for (const value of [form.primaryThreshold, form.secondaryThreshold]) {
    if (!value.trim() || !Number.isInteger(Number(value)) || Number(value) < 1 || Number(value) > 100)
      return 'threshold'
  }
  const attempts = Number(form.maxAttemptsPer24h)
  if (!form.maxAttemptsPer24h.trim() || !Number.isInteger(attempts) || attempts < 1 || attempts > 10)
    return 'attempts'
  const seconds = Number(form.cooldownSeconds)
  if (!form.cooldownSeconds.trim() || !Number.isInteger(seconds) || seconds < 3600 || seconds > 86400)
    return 'cooldown'
  return null
}

export function quotaPolicyWindows(policy: QuotaPolicy) {
  return (['primary', 'secondary'] as const).map(window => ({
    window,
    action: policy[window].action,
    thresholdPercent: policy[window].thresholdPercent,
  }))
}
