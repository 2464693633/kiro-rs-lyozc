import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  ProtocolCompatibilityConfig,
  ProtocolCompatibilityResponse,
} from '@/types/api'

const api = axios.create({
  baseURL: '/api/admin',
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

export async function getProtocolCompatibility(): Promise<ProtocolCompatibilityResponse> {
  const { data } = await api.get<ProtocolCompatibilityResponse>('/protocol-compatibility')
  return data
}

export async function setProtocolCompatibility(
  config: ProtocolCompatibilityConfig,
): Promise<ProtocolCompatibilityResponse> {
  const { data } = await api.put<ProtocolCompatibilityResponse>('/protocol-compatibility', config)
  return data
}
