import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { getTraces, getFailureStats, getUsageSourceStats } from '@/api/traces'
import type { TraceQuery } from '@/types/api'

/**
 * 请求链路查询 hook
 *
 * 切换筛选时保留旧数据避免闪烁。自动刷新由页面上的 AutoRefreshControl 统一管理，
 * 这样用户可以关闭或调整间隔，导航栏的全局刷新也能复用同一份 Query 缓存。
 * `enabled=false` 时不发请求（用于弹框未打开时的懒加载）。
 */
export function useTraces(query: TraceQuery, enabled = true) {
  return useQuery({
    queryKey: ['traces', query],
    queryFn: () => getTraces(query),
    enabled,
    staleTime: 10_000,
    placeholderData: keepPreviousData,
    refetchOnWindowFocus: false,
  })
}

/**
 * 按 usage 来源统计请求条数，用于确认本地计量配置的实际生效范围。
 *
 * 只有 simulated / adjusted 走了本地拆分；provider 是上游真值，固定上报与
 * 读取目标对它不生效。trace 关闭时总数为 0，此时无法判断生效范围。
 */
export function useUsageSourceStats(windowSecs = 86400) {
  return useQuery({
    queryKey: ['traces', 'usage-source-stats', windowSecs],
    queryFn: () => getUsageSourceStats(windowSecs),
    refetchInterval: 60_000,
    staleTime: 30_000,
    refetchOnWindowFocus: false,
    retry: 1,
  })
}

/** 按凭据的失败分类计数（鉴权/风控/其他），用于卡片分色展示 */
export function useFailureStats() {
  return useQuery({
    queryKey: ['traces', 'failure-stats'],
    queryFn: getFailureStats,
    refetchInterval: 30_000,
    staleTime: 10_000,
    refetchOnWindowFocus: false,
  })
}
