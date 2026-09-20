import type { Ref } from 'vue'
import type { getAccounts } from '@/api'

import { computed, ref, shallowRef, watch } from 'vue'
import { getAccountModels, updateAccount } from '@/api'
import { toast } from '@/components/base/BaseToast'
import { useAsyncAction } from '@/composables/useAsyncAction'
import { concurrencyLimitInput, parseAccountSchedulingForm } from '../utils/schedulingForm'

type AccountRow = Awaited<ReturnType<typeof getAccounts>>['items'][number]

export function useAccountEditor(options: {
  accounts: Ref<AccountRow[]>
  reloadAccounts: () => Promise<unknown>
  reloadGroups: () => Promise<unknown>
}) {
  const showEditModal = shallowRef(false)
  const editingAccountId = shallowRef<string | null>(null)
  const schedulingEnabled = shallowRef(true)
  const concurrencyLimit = shallowRef('')
  const weight = shallowRef('1')
  const proxyMode = shallowRef('preserve')
  const proxyId = shallowRef('')
  const selectedGroupIds = ref<string[]>([])
  const selectedModels = ref<string[]>([])
  const availableModels = ref<Array<{ id: string, label: string }>>([])
  const modelsLoading = shallowRef(false)
  let modelsRequestId = 0
  const saveAction = useAsyncAction()
  const saving = saveAction.loading
  const editingAccount = computed(() => {
    const accountId = editingAccountId.value
    return accountId
      ? options.accounts.value.find(account => account.id === accountId) ?? null
      : null
  })

  function open(account: AccountRow) {
    editingAccountId.value = account.id
    proxyMode.value = 'preserve'
    proxyId.value = ''
    schedulingEnabled.value = account.enabled
    concurrencyLimit.value = concurrencyLimitInput(account.concurrencyLimit)
    weight.value = String(account.weight)
    selectedGroupIds.value = account.groups.map(group => group.id)
    selectedModels.value = account.allowedModels ? [...account.allowedModels] : []
    availableModels.value = []
    showEditModal.value = true
    void loadModels(account.id)
  }

  async function loadModels(accountId: string) {
    const requestId = ++modelsRequestId
    modelsLoading.value = true
    try {
      const result = await getAccountModels({ accountId })
      if (requestId !== modelsRequestId)
        return
      const byId = new Map(result.models.map(model => [model.id, model]))
      for (const id of selectedModels.value)
        byId.set(id, byId.get(id) ?? { id, label: id })
      availableModels.value = [...byId.values()]
    }
    catch {
      if (requestId !== modelsRequestId)
        return
      const account = editingAccount.value
      availableModels.value = (account?.allowedModels ?? []).map(id => ({ id, label: id }))
    }
    finally {
      if (requestId === modelsRequestId)
        modelsLoading.value = false
    }
  }

  async function save() {
    const accountId = editingAccountId.value
    if (!accountId || saving.value)
      return
    const scheduling = parseAccountSchedulingForm(concurrencyLimit.value, weight.value)
    if (proxyMode.value === 'proxy' && !proxyId.value.trim()) {
      toast.warning('请选择已通过测试的代理')
      return
    }
    if (!scheduling.valid) {
      toast.warning(scheduling.message)
      return
    }

    await saveAction.run(async () => {
      await updateAccount({
        accountId,
        outboundProxyId: proxyMode.value === 'preserve' ? undefined : proxyMode.value === 'direct' ? '' : proxyId.value.trim(),
        enabled: schedulingEnabled.value,
        concurrencyLimit: scheduling.values.concurrencyLimit,
        weight: scheduling.values.weight,
        groupIds: [...new Set(selectedGroupIds.value)],
        allowedModels: [...new Set(selectedModels.value)],
      })
      showEditModal.value = false
      await Promise.all([options.reloadAccounts(), options.reloadGroups()])
      toast.success('账号已更新')
    }, { errorText: '账号更新失败' })
  }

  watch([showEditModal, saving], ([open, isSaving]) => {
    if (open || isSaving)
      return
    editingAccountId.value = null
    modelsRequestId += 1
    proxyMode.value = 'preserve'
    proxyId.value = ''
    schedulingEnabled.value = true
    concurrencyLimit.value = ''
    weight.value = '1'
    selectedGroupIds.value = []
    selectedModels.value = []
    availableModels.value = []
    modelsLoading.value = false
  })

  return {
    showEditModal,
    editingAccount,
    schedulingEnabled,
    concurrencyLimit,
    weight,
    proxyMode,
    proxyId,
    selectedGroupIds,
    selectedModels,
    availableModels,
    modelsLoading,
    saving,
    open,
    save,
  }
}
