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

// 更新两套引擎参数（后端落盘 + 热生效）
export function useSetCacheEnginesConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCacheEnginesConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['cacheEnginesConfig'] })
      queryClient.invalidateQueries({ queryKey: ['cacheEnginesStats'] })
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
