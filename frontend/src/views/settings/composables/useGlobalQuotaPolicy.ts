import type { GlobalQuotaPolicy, QuotaPolicy } from '@/api'
import { computed, ref, shallowRef } from 'vue'
import { getGlobalQuotaPolicy, updateGlobalQuotaPolicy } from '@/api'
import { ApiError } from '@/api/request'

import { t } from '@/i18n'
import { notification as ZNotification } from '@/utils/notification'
import { errorMessage } from '@/utils/operation'
import { policyFromForm, quotaPolicyErrorKey, quotaPolicyForm } from '../components/quota/model'

export function useGlobalQuotaPolicy() {
  const policy = shallowRef<GlobalQuotaPolicy>()
  const saved = shallowRef<QuotaPolicy>()
  const form = ref(quotaPolicyForm())
  const loading = shallowRef(true)
  const saving = shallowRef(false)
  const loadError = shallowRef('')
  const conflict = shallowRef(false)
  let loadSequence = 0

  const dirty = computed(() => saved.value !== undefined
    && JSON.stringify(policyFromForm(form.value)) !== JSON.stringify(saved.value))
  const validation = computed(() => quotaPolicyErrorKey(form.value))
  const draft = computed(() => policyFromForm(form.value))

  // 统一以服务端返回值为准；保留草稿时只更新版本和已保存值
  function apply(next: GlobalQuotaPolicy, keepDraft = false) {
    const draftForm = keepDraft ? form.value : quotaPolicyForm(next.policy)
    policy.value = next
    saved.value = next.policy
    form.value = draftForm
  }

  async function load(keepDraft = false) {
    const sequence = ++loadSequence
    loadError.value = ''
    try {
      const next = await getGlobalQuotaPolicy({ silent: true })
      if (sequence === loadSequence)
        apply(next, keepDraft)
    }
    catch (cause: unknown) {
      if (sequence !== loadSequence)
        return
      if (policy.value)
        ZNotification.error({ message: errorMessage(cause, t('settings.quotaPolicy.loadFailed')) })
      else
        loadError.value = errorMessage(cause)
    }
    finally {
      if (sequence === loadSequence)
        loading.value = false
    }
  }

  function reset() {
    if (!saved.value)
      return
    form.value = quotaPolicyForm(saved.value)
    conflict.value = false
  }

  async function save() {
    const current = policy.value
    if (!current || saving.value || !dirty.value || validation.value)
      return
    saving.value = true
    conflict.value = false
    try {
      const next = await updateGlobalQuotaPolicy({
        expectedRevision: current.revision,
        policy: policyFromForm(form.value),
      }, { silent: true })
      apply(next)
      ZNotification.success({ message: t('settings.quotaPolicy.saved') })
    }
    catch (cause: unknown) {
      // 版本冲突先读最新值并由管理员确认后重新保存，不自动覆盖他人修改
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

  return {
    policy,
    form,
    loading,
    saving,
    loadError,
    conflict,
    dirty,
    validation,
    draft,
    load,
    save,
    reset,
  }
}
