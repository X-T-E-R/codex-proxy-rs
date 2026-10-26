import type { Account, AccountOAuthHarness, ApiKeyConfiguration } from '@/api'
import { t } from '@/i18n'

export function resolveAccountOAuthHarness(account: Pick<Account, 'provider' | 'authenticationKind'> | null | undefined): AccountOAuthHarness | null {
  if (account?.provider !== 'openai')
    return null
  switch (account.authenticationKind) {
    case 'chatgpt_token_sharing':
      return 'opencode'
    case 'pi_openai':
      return 'pi'
    case 'oauth':
      return 'codex'
    default:
      return null
  }
}

export function isCodexOAuthAccount(account: Pick<Account, 'provider' | 'authenticationKind'> | null | undefined): boolean {
  return account?.provider === 'openai' && account.authenticationKind === 'oauth'
}

export function isOpenAiOAuthAccount(account: Pick<Account, 'provider' | 'authenticationKind'> | null | undefined): boolean {
  return account?.provider === 'openai' && resolveAccountOAuthHarness(account) !== null
}

export function canReauthorizeAccount(account: Pick<Account, 'provider' | 'authenticationKind'> | null | undefined): boolean {
  if (!account || !account.provider)
    return false
  if (account.provider === 'xai')
    return account.authenticationKind === 'oauth'
  if (account.provider === 'openai')
    return resolveAccountOAuthHarness(account) !== null
  return false
}

export function isOpenAiApiKeyAccount(account: Pick<Account, 'provider' | 'authenticationKind'> | null | undefined): boolean {
  return account?.provider === 'openai' && account.authenticationKind === 'api_key'
}

export interface ApiKeyAccountForm extends Omit<ApiKeyConfiguration, 'transport'> {
  name: string
  apiKey: string
  transport: ApiKeyConfiguration['transport'] | ''
}

export function emptyApiKeyAccountForm(): ApiKeyAccountForm {
  return { name: '', base_url: '', apiKey: '', transport: 'http' }
}

export function parseApiKeyConfiguration(value: Record<string, unknown> | undefined): ApiKeyConfiguration | undefined {
  if (!value || typeof value.base_url !== 'string' || (value.transport !== 'http' && value.transport !== 'prefer_websocket'))
    return undefined
  return { base_url: value.base_url, transport: value.transport }
}

export function apiKeyAccountError(form: ApiKeyAccountForm, editing = false): string | undefined {
  if (!editing && !form.name.trim())
    return t('accounts.credentials.nameRequired')
  if (!form.transport)
    return t('accounts.credentials.validation.transportRequired')
  if (form.base_url.trim().length > 2048)
    return t('accounts.credentials.validation.urlLength')
  try {
    const url = new URL(form.base_url)
    if (!['https:', 'http:'].includes(url.protocol) || !url.hostname || url.username || url.password || url.search || url.hash)
      return t('accounts.credentials.validation.urlFormat')
  }
  catch {
    return t('accounts.credentials.validation.urlRequired')
  }
  if (!editing && !form.apiKey)
    return t('accounts.credentials.apiKeyRequired')
  if (form.apiKey && (!/^[\x21-\x7E]+$/.test(form.apiKey) || form.apiKey.length > 16384))
    return t('accounts.credentials.validation.apiKeyFormat')
  return undefined
}
