import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import {
  getCacheEnginesConfig,
  getCacheEnginesStats,
  setCacheEnginesConfig,
} from '@/api/credentials'

// 获取两套缓存模拟引擎的参数
export function useCacheEnginesConfig(enabled = true) {
  return useQuery({
    queryKey: ['cacheEnginesConfig'],
    queryFn: getCacheEnginesConfig,
    enabled,
  })
}

// 更新四套引擎参数 + 全局倍率（后端落盘 + 热生效）
export function useSetCacheEnginesConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCacheEnginesConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['cacheEnginesConfig'] })
      queryClient.invalidateQueries({ queryKey: ['cacheEnginesStats'] })
      // payload 的 global 段与 /config/token-inflation 是同一份存储。
      // 顶栏齿轮的「非默认」状态点读的是 tokenInflationConfig，不一起失效
      // 就会在这里改完倍率后继续显示旧状态。
      queryClient.invalidateQueries({ queryKey: ['tokenInflationConfig'] })
    },
  })
}

// 两套引擎的运行计数器。仅在弹窗打开时轮询，避免常驻请求。
export function useCacheEnginesStats(enabled = true) {
  return useQuery({
    queryKey: ['cacheEnginesStats'],
    queryFn: getCacheEnginesStats,
    enabled,
    refetchInterval: enabled ? 5000 : false,
  })
}
