import type { AccountQuotaPolicy, AccountQuotaPolicyMode, QuotaPolicy } from '@/api'
import { computed, onScopeDispose, ref, shallowRef, watch } from 'vue'
import { getAccountQuotaPolicy, updateAccountQuotaPolicy } from '@/api'
import { ApiError } from '@/api/request'

import { t } from '@/i18n'
import { notification as ZNotification } from '@/utils/notification'
import { errorMessage } from '@/utils/operation'
import { policyFromForm, quotaPolicyErrorKey, quotaPolicyForm } from '../../settings/components/quota/model'

interface AccountQuotaDraft {
  mode: AccountQuotaPolicyMode
  policy: ReturnType<typeof quotaPolicyForm>
}

export function useAccountQuotaPolicy(options: { accountId: () => string }) {
  const view = shallowRef<AccountQuotaPolicy>()
  const saved = shallowRef<AccountQuotaDraft>()
  const mode = shallowRef<AccountQuotaPolicyMode>('inherit')
  const form = ref(quotaPolicyForm())
  const loading = shallowRef(true)
  const saving = shallowRef(false)
  const loadError = shallowRef('')
  const conflict = shallowRef(false)
  let sequence = 0
  let controller: AbortController | undefined

  const dirty = computed(() => saved.value !== undefined
    && (mode.value !== saved.value.mode
      || JSON.stringify(quotaPolicyForm(policyFromForm(form.value))) !== JSON.stringify(saved.value.policy)))
  const validation = computed(() => quotaPolicyErrorKey(form.value))
  const effectivePolicy = computed<QuotaPolicy | undefined>(() => view.value?.effectivePolicy)
  const source = computed(() => view.value?.source)
  const status = computed(() => view.value?.status)

  // 统一以服务端返回值为准；保留草稿时只更新版本与已保存值
  function apply(next: AccountQuotaPolicy, keepDraft = false) {
    const draftMode = keepDraft ? mode.value : next.mode
    const draftForm = keepDraft
      ? form.value
      : quotaPolicyForm(next.mode === 'custom' ? next.policy ?? next.effectivePolicy : next.effectivePolicy)
    view.value = next
    saved.value = {
      mode: next.mode,
      policy: quotaPolicyForm(next.mode === 'custom' ? next.policy ?? next.effectivePolicy : next.effectivePolicy),
    }
    mode.value = draftMode
    form.value = draftForm
  }

  async function load(keepDraft = false) {
    const accountId = options.accountId()
    const current = ++sequence
    controller?.abort()
    const next = new AbortController()
    controller = next
    loadError.value = ''
    if (!view.value)
      loading.value = true
    try {
      const result = await getAccountQuotaPolicy({ accountId }, { silent: true, signal: next.signal })
      if (current === sequence)
        apply(result, keepDraft)
    }
    catch (cause: unknown) {
      if (current !== sequence || next.signal.aborted)
        return
      if (view.value)
        ZNotification.error({ message: errorMessage(cause, t('accounts.quotaPolicy.loadFailed')) })
      else
        loadError.value = errorMessage(cause)
    }
    finally {
      if (current === sequence)
        loading.value = false
    }
  }

  function reset() {
    if (!saved.value)
      return
    mode.value = saved.value.mode
    form.value = quotaPolicyForm(policyFromForm(saved.value.policy))
    conflict.value = false
  }

  async function save() {
    const current = view.value
    if (!current || saving.value || !dirty.value || validation.value)
      return
    saving.value = true
    conflict.value = false
    try {
      const next = await updateAccountQuotaPolicy({
        accountId: current.accountId,
        expectedRevision: current.revision,
        mode: mode.value,
        policy: mode.value === 'custom' ? policyFromForm(form.value) : null,
      }, { silent: true })
      apply(next)
      ZNotification.success({ message: t('accounts.quotaPolicy.saved') })
    }
    catch (cause: unknown) {
      if (cause instanceof ApiError && cause.status === 409) {
        conflict.value = true
        await load(true)
      }
      else {
        ZNotification.error({ message: errorMessage(cause) })
      }
    }
    finally {
      saving.value = false
    }
  }

  watch(options.accountId, () => {
    controller?.abort()
    view.value = undefined
    saved.value = undefined
    mode.value = 'inherit'
    form.value = quotaPolicyForm()
    conflict.value = false
    void load()
  }, { immediate: true })

  onScopeDispose(() => {
    sequence += 1
    controller?.abort()
  })

  return {
    view,
    mode,
    form,
    loading,
    saving,
    loadError,
    conflict,
    dirty,
    validation,
    effectivePolicy,
    source,
    status,
    load,
    save,
    reset,
  }
}
