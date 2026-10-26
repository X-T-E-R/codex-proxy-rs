import type { ApiKeyAccountForm } from './upstreamApiKey'
import type { Account, AccountImportSettings, AccountModelAccess, AccountOAuthHarness } from '@/api'
import { t } from '@/i18n'
import { formatProviderLabel } from '@/utils/providers'
import { accountModelAccessError } from './modelAccess'
import { parseAccountSchedulingForm } from './schedulingForm'
import { emptyApiKeyAccountForm } from './upstreamApiKey'

export type AccountCreateSource = { kind: 'bundle' } | { kind: 'provider', id: string }
export type AccountImportMode = 'oauth' | 'api_key' | 'access_token' | 'refresh_token' | 'json'
type AccountImportInputMode = 'access_token' | 'refresh_token' | 'json'

export interface AccountCreateForm {
  source: AccountCreateSource | null
  apiKey: ApiKeyAccountForm
  oauthHarness: AccountOAuthHarness
  notes: string
  enabled: boolean
  concurrencyLimit: string
  weight: string
  modelAccess?: AccountModelAccess
  groupIds: string[]
  step: 'settings' | 'import'
  mode: AccountImportMode
  importTexts: Record<AccountImportInputMode, string>
  proxyMode: string
  proxyId: string
}

export function emptyAccountCreateForm(): AccountCreateForm {
  return {
    source: null,
    apiKey: emptyApiKeyAccountForm(),
    oauthHarness: 'codex',
    notes: '',
    enabled: true,
    concurrencyLimit: '',
    weight: '1',
    modelAccess: undefined,
    groupIds: [],
    step: 'settings',
    mode: 'oauth',
    importTexts: { access_token: '', refresh_token: '', json: '' },
    proxyMode: 'direct',
    proxyId: '',
  }
}

export function accountCreateProvider(form: AccountCreateForm) {
  return form.source?.kind === 'provider' ? form.source.id : undefined
}

export function accountCreateSourceKey(form: AccountCreateForm) {
  return form.source?.kind === 'provider' ? `provider:${form.source.id}` : form.source?.kind ?? ''
}

export function accountProxyError(form: AccountCreateForm): string | undefined {
  if (form.proxyMode !== 'proxy')
    return undefined
  if (!form.proxyId.trim())
    return t('accounts.batchEdit.proxyRequired')
  return undefined
}

export function accountImportSettings(form: AccountCreateForm): AccountImportSettings {
  const scheduling = parseAccountSchedulingForm(form.concurrencyLimit, form.weight)
  if (!scheduling.valid)
    throw new Error(scheduling.message)
  const modelError = accountModelAccessError(form.modelAccess)
  if (modelError)
    throw new Error(modelError)
  return {
    modelAccess: form.modelAccess ? { ...form.modelAccess, models: [...form.modelAccess.models] } : undefined,
    enabled: form.enabled,
    ...scheduling.values,
    groupIds: [...new Set(form.groupIds)],
    notes: form.notes.trim() || undefined,
  }
}

export function accountOAuthHarnessOptions() {
  return [
    { label: t('accounts.onboarding.harnesses.codex'), value: 'codex' as const },
    { label: t('accounts.onboarding.harnesses.opencode'), value: 'opencode' as const },
    { label: t('accounts.onboarding.harnesses.pi'), value: 'pi' as const },
  ]
}

export function buildOAuthStartPayload(input: {
  account: (Pick<Account, 'id' | 'name' | 'email'> & Partial<Pick<Account, 'provider'>>) | null | undefined
  form: Pick<AccountCreateForm, 'oauthHarness' | 'proxyMode' | 'proxyId'>
  provider: string
}) {
  const isReauth = Boolean(input.account)
  return {
    provider: input.provider,
    name: input.account?.name || input.account?.email || t('accounts.onboarding.accountName', { provider: formatProviderLabel(input.provider) }),
    accountId: input.account?.id,
    harness: !isReauth && input.provider === 'openai' ? input.form.oauthHarness : undefined,
    outboundProxyId: input.account ? undefined : input.form.proxyMode === 'proxy' ? input.form.proxyId.trim() : undefined,
  }
}

export function accountImportModes(provider: string | undefined) {
  if (provider === 'openai') {
    return [
      { label: 'OAuth', value: 'oauth' as const },
      { label: 'API Key', value: 'api_key' as const },
      { label: 'AT', value: 'access_token' as const },
      { label: 'RT', value: 'refresh_token' as const },
      { label: t('accounts.onboarding.accountFile'), value: 'json' as const },
    ]
  }
  if (provider === 'xai')
    return [{ label: 'OAuth', value: 'oauth' as const }, { label: t('accounts.onboarding.accountFile'), value: 'json' as const }]
  return []
}
