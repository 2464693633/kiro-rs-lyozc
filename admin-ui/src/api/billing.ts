import axios from 'axios'
import { storage } from '@/lib/storage'
import type { BillingComparisonResponse, BillingConfig, StatsFilter, StatsTimeFilter } from '@/types/api'

const api = axios.create({
  baseURL: '/api/admin',
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const key = storage.getApiKey()
  if (key) config.headers['x-api-key'] = key
  return config
})

export async function getBillingConfig(): Promise<BillingConfig> {
  const { data } = await api.get<BillingConfig>('/config/billing')
  return data
}

export async function setBillingConfig(config: BillingConfig): Promise<BillingConfig> {
  const { data } = await api.put<BillingConfig>('/config/billing', config)
  return data
}

export async function getBillingComparison(
  time: StatsTimeFilter,
  filter?: StatsFilter,
): Promise<BillingComparisonResponse> {
  const { data } = await api.get<BillingComparisonResponse>('/stats/billing', {
    params: {
      ...time,
      ...(filter?.keyId !== undefined ? { keyId: filter.keyId } : {}),
      ...(filter?.group ? { group: filter.group } : {}),
    },
  })
  return data
}
