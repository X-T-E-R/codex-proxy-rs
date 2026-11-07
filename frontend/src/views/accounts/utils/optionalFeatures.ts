export const ACCOUNT_OPTIONAL_FEATURES = [
  {
    id: 'capacity_freeze',
    labelKey: 'settings.autoDisable.overloadProtection',
    providers: ['openai'],
    oauthOnly: false,
  },
  {
    id: 'account_warmup',
    labelKey: 'settings.warmup.title',
    providers: ['openai'],
    oauthOnly: true,
  },
  {
    id: 'cyber_session_block',
    labelKey: 'settings.cyber.title',
    providers: ['openai', 'xai'],
    oauthOnly: false,
  },
] as const

export type AccountOptionalFeatureId = (typeof ACCOUNT_OPTIONAL_FEATURES)[number]['id']

export function applicableOptionalFeatures(provider?: string, authenticationKind?: string) {
  return ACCOUNT_OPTIONAL_FEATURES.filter((feature) => {
    if (!provider)
      return true
    if (!feature.providers.some(item => item === provider))
      return false
    if (feature.oauthOnly && authenticationKind && authenticationKind !== 'oauth')
      return false
    return true
  })
}
