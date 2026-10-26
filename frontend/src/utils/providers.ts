import type { Component } from 'vue'
import { Key, LinkAlt, Openai, Xai } from '@boxicons/vue'
import { Fingerprint, Plug } from '@lucide/vue'
import { t } from '@/i18n'

const PROVIDER_DISPLAY_NAMES = {
  openai: 'OpenAI',
  xai: 'xAI',
} as const

export const PROVIDER_IDS = Object.keys(PROVIDER_DISPLAY_NAMES) as Array<keyof typeof PROVIDER_DISPLAY_NAMES>

type SupportedProvider = keyof typeof PROVIDER_DISPLAY_NAMES

export function isSupportedProvider(value: unknown): value is SupportedProvider {
  return typeof value === 'string' && Object.hasOwn(PROVIDER_DISPLAY_NAMES, value)
}

export function formatProviderLabel(value?: string | null, fallback = '—') {
  const normalized = value?.trim()
  const provider = normalized?.toLowerCase()
  return isSupportedProvider(provider) ? PROVIDER_DISPLAY_NAMES[provider] : (normalized || fallback)
}

export function providerIcon(value: string): Component {
  switch (value.trim().toLowerCase()) {
    case 'openai': return Openai
    case 'xai': return Xai
    default: return Plug
  }
}

export function formatAuthenticationLabel(kind?: string | null, provider?: string | null) {
  const normalizedKind = kind?.trim()
  switch (normalizedKind?.toLowerCase()) {
    case 'chatgpt_token_sharing':
      return 'OpenCode OAuth'
    case 'pi_openai':
      return 'pi OAuth'
    case 'oauth':
      if (provider?.trim().toLowerCase() === 'openai')
        return 'Codex OAuth'
      return 'OAuth'
    case 'api_key': return 'API Key'
    default: return normalizedKind ? t('common.providers.customAuth', { normalized: normalizedKind }) : t('common.providers.unknownAuth')
  }
}

export function authenticationIcon(value?: string | null): Component | undefined {
  switch (value?.trim().toLowerCase()) {
    case 'oauth':
    case 'chatgpt_token_sharing':
    case 'pi_openai':
      return LinkAlt
    case 'api_key': return Key
    case undefined:
    case '': return undefined
    default: return Fingerprint
  }
}
