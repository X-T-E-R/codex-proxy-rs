import type { AccountPriorityMode, AccountPriorityPolicy } from '@/api'
import { computed, onScopeDispose, shallowRef, watch } from 'vue'
import { getAccountPriorityPolicy, updateAccountPriorityPolicy } from '@/api'
import { ApiError } from '@/api/request'

import { t } from '@/i18n'
import { notification as ZNotification } from '@/utils/notification'
import { errorMessage } from '@/utils/operation'

interface AccountPriorityDraft {
  mode: AccountPriorityMode
  /** null 表示继承全局 */
  autoOffOnQuotaReset: boolean | null
}

type PrioritySyncListener = (next: AccountPriorityPolicy) => void

const syncListeners = new Map<string, Set<PrioritySyncListener>>()

function registerPriorityListener(accountId: string, listener: PrioritySyncListener) {
  let set = syncListeners.get(accountId)
  if (!set) {
    set = new Set()
    syncListeners.set(accountId, set)
  }
  set.add(listener)
}

function unregisterPriorityListener(accountId: string, listener: PrioritySyncListener) {
  const set = syncListeners.get(accountId)
  if (set) {
    set.delete(listener)
    if (set.size === 0)
      syncListeners.delete(accountId)
  }
}

function broadcastPriorityPolicy(accountId: string, next: AccountPriorityPolicy, sourceListener: PrioritySyncListener) {
  const set = syncListeners.get(accountId)
  if (set) {
    for (const listener of set) {
      if (listener !== sourceListener)
        listener(next)
    }
  }
}

export function useAccountPriorityPolicy(options: { accountId: () => string }) {
  const policy = shallowRef<AccountPriorityPolicy>()
  const saved = shallowRef<AccountPriorityDraft>()
  const mode = shallowRef<AccountPriorityMode>('inherit')
  const autoOffOnQuotaReset = shallowRef<boolean | null>(null)
  const loading = shallowRef(true)
  const saving = shallowRef(false)
  const loadError = shallowRef('')
  const conflict = shallowRef(false)
  let sequence = 0
  let controller: AbortController | undefined

  const dirty = computed(() => saved.value !== undefined
    && (mode.value !== saved.value.mode || autoOffOnQuotaReset.value !== saved.value.autoOffOnQuotaReset))
  const effectiveMode = computed(() => policy.value?.effectiveMode ?? 'default')
  const effectiveAutoOffOnQuotaReset = computed(() => policy.value?.effectiveAutoOffOnQuotaReset ?? false)
  const status = computed(() => policy.value?.status ?? null)

  function onRemoteUpdate(next: AccountPriorityPolicy) {
    apply(next, dirty.value)
  }

  function apply(next: AccountPriorityPolicy, keepDraft = false) {
    const draft = keepDraft
      ? { mode: mode.value, autoOffOnQuotaReset: autoOffOnQuotaReset.value }
      : { mode: next.mode, autoOffOnQuotaReset: next.autoOffOnQuotaReset }
    policy.value = next
    saved.value = { mode: next.mode, autoOffOnQuotaReset: next.autoOffOnQuotaReset }
    mode.value = draft.mode
    autoOffOnQuotaReset.value = draft.autoOffOnQuotaReset
  }

  async function load(keepDraft = false) {
    const accountId = options.accountId()
    const current = ++sequence
    controller?.abort()
    const next = new AbortController()
    controller = next
    loadError.value = ''
    if (!policy.value)
      loading.value = true
    try {
      const result = await getAccountPriorityPolicy({ accountId }, { silent: true, signal: next.signal })
      if (current === sequence)
        apply(result, keepDraft)
    }
    catch (cause: unknown) {
      if (current !== sequence || next.signal.aborted)
        return
      // 已有读值时保留面板与草稿，只有首次加载失败才整块报错
      if (policy.value)
        ZNotification.error({ message: errorMessage(cause, t('accounts.priority.loadFailed')) })
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
    autoOffOnQuotaReset.value = saved.value.autoOffOnQuotaReset
    conflict.value = false
  }

  async function save() {
    const current = policy.value
    if (!current || saving.value || !dirty.value)
      return
    saving.value = true
    conflict.value = false
    try {
      const next = await updateAccountPriorityPolicy({
        accountId: current.accountId,
        expectedRevision: current.revision,
        mode: mode.value,
        autoOffOnQuotaReset: autoOffOnQuotaReset.value,
      }, { silent: true })
      apply(next)
      broadcastPriorityPolicy(current.accountId, next, onRemoteUpdate)
      ZNotification.success({ message: t('accounts.priority.saved') })
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

  let boundAccountId: string | undefined

  watch(options.accountId, (newId, oldId) => {
    if (oldId)
      unregisterPriorityListener(oldId, onRemoteUpdate)
    boundAccountId = newId
    if (newId)
      registerPriorityListener(newId, onRemoteUpdate)
    controller?.abort()
    policy.value = undefined
    saved.value = undefined
    mode.value = 'inherit'
    autoOffOnQuotaReset.value = null
    conflict.value = false
    void load()
  }, { immediate: true })

  onScopeDispose(() => {
    sequence += 1
    controller?.abort()
    if (boundAccountId)
      unregisterPriorityListener(boundAccountId, onRemoteUpdate)
  })

  return {
    policy,
    mode,
    autoOffOnQuotaReset,
    loading,
    saving,
    loadError,
    conflict,
    dirty,
    effectiveMode,
    effectiveAutoOffOnQuotaReset,
    status,
    load,
    save,
    reset,
  }
}
