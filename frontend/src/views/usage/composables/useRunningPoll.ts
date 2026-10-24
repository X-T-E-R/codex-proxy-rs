import { onScopeDispose } from 'vue'

interface RunningPollOptions {
  /** 两次请求起点之间的间隔，慢请求耗时从间隔里扣掉 */
  intervalMs?: number
  /** 是否仍有需要跟进的运行中记录 */
  shouldRun: () => boolean
  refresh: () => Promise<void>
}

/**
 * 运行中的用量记录会在网关侧自然转终态，页面需要在可见时跟进。
 * 轮询串行：上一次请求结束后才排下一次，慢请求不会叠加；
 * 页面隐藏时停表，回到前台立即补一次再回到正常间隔。
 */
export function useRunningPoll(options: RunningPollOptions) {
  const intervalMs = options.intervalMs ?? 5000
  let timer: number | undefined
  let active = false
  let inFlight = false

  function clearTimer() {
    if (timer !== undefined) {
      window.clearTimeout(timer)
      timer = undefined
    }
  }

  function schedule(delay = intervalMs) {
    clearTimer()
    if (!active || inFlight || document.hidden || !options.shouldRun())
      return
    timer = window.setTimeout(() => void tick(), Math.max(0, delay))
  }

  async function tick() {
    timer = undefined
    if (!active || inFlight || document.hidden || !options.shouldRun())
      return
    const startedAt = Date.now()
    inFlight = true
    try {
      await options.refresh()
    }
    finally {
      inFlight = false
    }
    // 刷新后可能已转终态或页面已隐藏，交给 schedule 重新判定；
    // 慢请求吃掉的时间从间隔里扣掉，请求起点之间仍保持固定节奏
    schedule(intervalMs - (Date.now() - startedAt))
  }

  function onVisibilityChange() {
    if (!active)
      return
    if (document.hidden) {
      clearTimer()
      return
    }
    // 隐藏期间的终态变化不能被漏掉，回前台立即补一次
    clearTimer()
    if (!inFlight)
      void tick()
  }

  function start() {
    if (active)
      return
    active = true
    document.addEventListener('visibilitychange', onVisibilityChange)
    schedule()
  }

  function stop() {
    if (!active)
      return
    active = false
    document.removeEventListener('visibilitychange', onVisibilityChange)
    clearTimer()
  }

  onScopeDispose(stop)

  return { start, stop }
}
