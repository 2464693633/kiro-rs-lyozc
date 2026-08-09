import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  listClientKeys,
  createClientKey,
  deleteClientKey,
  updateClientKey,
  setClientKeyDisabled,
  resetClientKeyStats,
  rotateClientKey,
  revealClientKey,
} from '@/api/client-keys'
import type { CreateClientKeyRequest, UpdateClientKeyRequest } from '@/types/api'

export function useClientKeys() {
  return useQuery({
    queryKey: ['client-keys'],
    queryFn: listClientKeys,
    refetchInterval: 30000,
  })
}

export function useCreateClientKey() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (req: CreateClientKeyRequest) => createClientKey(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useDeleteClientKey() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteClientKey(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useUpdateClientKey() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateClientKeyRequest }) =>
      updateClientKey(id, req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useSetClientKeyDisabled() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setClientKeyDisabled(id, disabled),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useResetClientKeyStats() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetClientKeyStats(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useRotateClientKey() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => rotateClientKey(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

/**
 * 查看既有 Key 的明文。
 *
 * 用 mutation 而非 query：这是点击触发的一次性动作，不该被缓存、更不该被自动轮询
 * 重取（明文留在 query cache 里没有意义）。也不需要 invalidate —— 只读不改状态。
 */
export function useRevealClientKey() {
  return useMutation({
    mutationFn: (id: number) => revealClientKey(id),
  })
}
