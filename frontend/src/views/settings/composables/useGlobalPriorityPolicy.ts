import type { FastMode, GlobalPriorityPolicy } from '@/api'
import { computed, shallowRef } from 'vue'
import { getGlobalPriorityPolicy, updateGlobalPriorityPolicy } from '@/api'
import { ApiError } from '@/api/request'

import { t } from '@/i18n'
import { notification as ZNotification } from '@/utils/notification'
import { errorMessage } from '@/utils/operation'

export function useGlobalPriorityPolicy() {
  const policy = shallowRef<GlobalPriorityPolicy>()
  const savedMode = shallowRef<FastMode>()
  const savedAutoOff = shallowRef<boolean>()
  const mode = shallowRef<FastMode>('default')
  const autoOffOnQuotaReset = shallowRef(false)
  const loading = shallowRef(true)
  const saving = shallowRef(false)
  const loadError = shallowRef('')
  const conflict = shallowRef(false)
  let loadSequence = 0

  const dirty = computed(() => savedMode.value !== undefined
    && (mode.value !== savedMode.value || autoOffOnQuotaReset.value !== savedAutoOff.value))

  // 统一以服务端返回值为准；保留草稿时只更新版本和已保存值
  function apply(next: GlobalPriorityPolicy, keepDraft = false) {
    const draftMode = keepDraft ? mode.value : next.mode
    const draftAutoOff = keepDraft ? autoOffOnQuotaReset.value : next.autoOffOnQuotaReset
    policy.value = next
    savedMode.value = next.mode
    savedAutoOff.value = next.autoOffOnQuotaReset
    mode.value = draftMode
    autoOffOnQuotaReset.value = draftAutoOff
  }

  async function load(keepDraft = false) {
    const sequence = ++loadSequence
    loadError.value = ''
    try {
      const next = await getGlobalPriorityPolicy({ silent: true })
      if (sequence === loadSequence)
        apply(next, keepDraft)
    }
    catch (cause: unknown) {
      if (sequence !== loadSequence)
        return
      // 已有读值时保留面板与草稿，只有首次加载失败才整卡报错
      if (policy.value)
        ZNotification.error({ message: errorMessage(cause, t('settings.priority.loadFailed')) })
      else
        loadError.value = errorMessage(cause)
    }
    finally {
      if (sequence === loadSequence)
        loading.value = false
    }
  }

  function reset() {
    if (savedMode.value === undefined || savedAutoOff.value === undefined)
      return
    mode.value = savedMode.value
    autoOffOnQuotaReset.value = savedAutoOff.value
    conflict.value = false
  }

  async function save() {
    const current = policy.value
    if (!current || saving.value || !dirty.value)
      return
    saving.value = true
    conflict.value = false
    try {
      const next = await updateGlobalPriorityPolicy({
        expectedRevision: current.revision,
        mode: mode.value,
        autoOffOnQuotaReset: autoOffOnQuotaReset.value,
      }, { silent: true })
      apply(next)
      ZNotification.success({ message: t('settings.priority.saved') })
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
    mode,
    autoOffOnQuotaReset,
    loading,
    saving,
    loadError,
    conflict,
    dirty,
    load,
    save,
    reset,
  }
}
