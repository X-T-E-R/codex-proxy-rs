export interface AccountSchedulingValues {
  concurrencyLimit: number | null
  weight: number
}

const MAX_ACCOUNT_CONCURRENCY = 4_294_967_295

type AccountSchedulingParseResult
  = | { valid: true, values: AccountSchedulingValues }
    | { valid: false, message: string }

export function parseAccountSchedulingForm(
  concurrencyLimit: string,
  weight: string,
): AccountSchedulingParseResult {
  const limitText = concurrencyLimit.trim()
  const parsedLimit = limitText === '' ? null : Number(limitText)
  if (
    parsedLimit !== null
    && (!Number.isSafeInteger(parsedLimit)
      || parsedLimit < 1
      || parsedLimit > MAX_ACCOUNT_CONCURRENCY)
  ) {
    return { valid: false, message: `并发限制必须留空，或为 1 到 ${MAX_ACCOUNT_CONCURRENCY} 的整数` }
  }

  const parsedWeight = Number(weight.trim())
  if (!Number.isSafeInteger(parsedWeight) || parsedWeight < 1 || parsedWeight > 100) {
    return { valid: false, message: '权重必须是 1 到 100 的整数' }
  }

  return {
    valid: true,
    values: {
      concurrencyLimit: parsedLimit,
      weight: parsedWeight,
    },
  }
}

export function concurrencyLimitInput(value: number | null) {
  return value === null ? '' : String(value)
}

const MAX_OVERLOAD_COOLDOWN = 4_294_967_295

export type AccountOverloadCooldownMode = 'inherit' | 'disabled' | 'custom'

export interface AccountOverloadCooldownPayload {
  mode: AccountOverloadCooldownMode
  threshold: number | null
  seconds: number | null
}

type OverloadCooldownParseResult
  = | { valid: true, value: AccountOverloadCooldownPayload }
    | { valid: false, message: string }

// 账号级过载冷号覆盖：custom 必须携带两个正整数，其他模式不携带数值。
export function parseOverloadCooldownForm(
  mode: string,
  threshold: string,
  seconds: string,
): OverloadCooldownParseResult {
  if (mode !== 'custom') {
    const normalized: AccountOverloadCooldownMode = mode === 'disabled' ? 'disabled' : 'inherit'
    return { valid: true, value: { mode: normalized, threshold: null, seconds: null } }
  }

  const parsedThreshold = parsePositiveInteger(threshold)
  if (parsedThreshold === null) {
    return { valid: false, message: `连续过载次数必须是 1 到 ${MAX_OVERLOAD_COOLDOWN} 的整数` }
  }
  const parsedSeconds = parsePositiveInteger(seconds)
  if (parsedSeconds === null) {
    return { valid: false, message: `冷号时长必须是 1 到 ${MAX_OVERLOAD_COOLDOWN} 的整数` }
  }
  return {
    valid: true,
    value: { mode: 'custom', threshold: parsedThreshold, seconds: parsedSeconds },
  }
}

function parsePositiveInteger(value: string): number | null {
  const parsed = Number(value.trim())
  if (!Number.isSafeInteger(parsed) || parsed < 1 || parsed > MAX_OVERLOAD_COOLDOWN)
    return null
  return parsed
}
