/* eslint-disable test/no-import-node-test -- 前端未依赖 Vitest，此处使用 Node 原生测试入口 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import path from 'node:path'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { Script } from 'node:vm'
import ts from 'typescript'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const source = readFileSync(path.join(root, 'src/views/accounts/composables/useAccountConnectionTest.ts'), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } })

function connectionTestProjection(locale) {
  const messages = JSON.parse(readFileSync(path.join(root, `src/i18n/locales/${locale}/accounts.json`), 'utf8')).accounts.connectionTest.errors
  const module = { exports: {} }
  const reactive = (initial) => {
    let value = initial
    const listeners = []
    return {
      get value() { return value },
      set value(next) {
        value = next
        listeners.forEach(listener => listener(next))
      },
      subscribe(listener) { listeners.push(listener) },
    }
  }
  const streamData = reactive(null)
  const vue = {
    computed: fn => ({ get value() { return fn() } }),
    shallowRef: reactive,
    ref: reactive,
    watch: (source, listener) => {
      if (!Array.isArray(source))
        source.subscribe(listener)
    },
    onBeforeUnmount: () => {},
  }
  const testing = new Set()
  const mock = {
    vue,
    '@vueuse/core': { useEventSource: () => ({ data: streamData, error: reactive(null), eventSource: reactive({}), open() {}, close() {} }) },
    '@/composables/useIdSet': { useIdSet: () => ({ ids: reactive([]), add: id => testing.add(id), remove: id => testing.delete(id), has: id => testing.has(id) }) },
    '@/composables/useRequestState': { useRequestState: () => ({ loading: reactive(false), invalidate() {} }) },
    '@/api/constants': { API_BASE_URL: 'http://localhost' },
    '@/i18n': { t: key => messages[key.slice('accounts.connectionTest.errors.'.length)] ?? key },
    '@/utils/operation': { withMinimumDuration: fn => fn() },
    'es-toolkit': { clamp: value => Math.max(0, value) },
  }
  const load = name => mock[name] ?? {}
  new Script(`(function(require, module, exports) { ${outputText}\n})`, { filename: 'useAccountConnectionTest.ts' })
    .runInThisContext()(load, module, module.exports)
  return { ...module.exports, streamData }
}

for (const locale of ['en', 'zh-Hans', 'zh-Hant']) {
  test(`${locale} consumes upstream refusal SSE with localized summary and safe details`, async () => {
    const { useAccountConnectionTest, streamData } = connectionTestProjection(locale)
    const messages = JSON.parse(readFileSync(path.join(root, `src/i18n/locales/${locale}/accounts.json`), 'utf8')).accounts.connectionTest.errors
    const state = useAccountConnectionTest({ reload: async () => {} })
    state.testingAccount.value = { id: 'account' }
    state.showConnectionTestModal.value = true
    state.connectionTestSelectedModel.value = 'gpt'
    const pending = state.handleTestConnection()
    const event = {
      type: 'error',
      source: 'upstream',
      gatewayErrorCode: 'upstream_permission_denied',
      providerErrorKind: 'permission_denied',
      diagnosticCode: 'subscription_sharing_ineligible',
      upstreamStatus: 403,
      error: 'upstream denied this request',
      occurredAtDisplay: 'now',
      timeDisplay: 'now',
    }
    streamData.value = { raw: JSON.stringify(event) }
    await pending
    assert.equal(state.connectionTestStatus.value, 'error')
    assert.equal(state.connectionTestError.value, messages.subscriptionSharingIneligible)
    const detail = JSON.parse(state.connectionTestLogs.value.at(-1).detail)
    assert.equal(detail.providerErrorKind, 'permission_denied')
    assert.equal(detail.diagnosticCode, 'subscription_sharing_ineligible')
    assert.equal(detail.upstreamStatus, 403)
    assert.equal(detail.upstreamBody, null)

    const fallback = connectionTestProjection(locale).connectionTestFailureText
    assert.equal(fallback({ ...event, diagnosticCode: null }), messages.upstreamPermissionDenied)
    assert.notEqual(fallback({ ...event, gatewayErrorCode: 'upstream_unavailable' }), messages.subscriptionSharingIneligible)
  })
}
