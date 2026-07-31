import { keepPreviousData, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { getBillingComparison, getBillingConfig, setBillingConfig } from '@/api/billing'
import type { StatsFilter, StatsTimeFilter } from '@/types/api'

export function useBillingConfig() {
  return useQuery({ queryKey: ['billingConfig'], queryFn: getBillingConfig })
}

export function useSetBillingConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setBillingConfig,
    onSuccess: (data) => queryClient.setQueryData(['billingConfig'], data),
  })
}

export function useBillingComparison(time: StatsTimeFilter, filter?: StatsFilter) {
  return useQuery({
    queryKey: [
      'stats',
      'billing',
      time,
      filter?.keyId ?? 'all',
      filter?.group ?? 'all',
      filter?.credentialId ?? 'all',
    ],
    queryFn: () => getBillingComparison(time, filter),
    staleTime: 25_000,
    refetchInterval: 30_000,
    placeholderData: keepPreviousData,
    refetchOnWindowFocus: false,
  })
}
