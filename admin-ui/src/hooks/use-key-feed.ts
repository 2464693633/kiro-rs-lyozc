import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import {
  getKeyFeedConfig,
  pullKeyFeed,
  setKeyFeedConfig,
} from '@/api/credentials'

/** 订阅源配置。仅在弹窗打开时拉取，避免常驻请求。 */
export function useKeyFeedConfig(enabled = true) {
  return useQuery({
    queryKey: ['keyFeedConfig'],
    queryFn: getKeyFeedConfig,
    enabled,
  })
}

/** 更新订阅源配置（后端落盘 + 热生效） */
export function useSetKeyFeedConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setKeyFeedConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['keyFeedConfig'] })
    },
  })
}

/**
 * 手动触发一次拉取。
 *
 * 成功后同时失效凭据列表 —— 新号位入库后凭据面板要立刻能看到。
 */
export function usePullKeyFeed() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: pullKeyFeed,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['keyFeedConfig'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}
