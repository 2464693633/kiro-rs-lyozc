import axios from 'axios'
import { storage } from '@/lib/storage'
import type { FailureStatsMap, TracePage, TraceQuery } from '@/types/api'

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

export async function getTraces(query: TraceQuery): Promise<TracePage> {
  const params: Record<string, string> = {}
  if (query.status) params.status = query.status
  if (query.errorType) params.errorType = query.errorType
  if (query.credentialId != null) params.credentialId = String(query.credentialId)
  if (query.keyId != null) params.keyId = String(query.keyId)
  if (query.failedAttemptCredentialId != null)
    params.failedAttemptCredentialId = String(query.failedAttemptCredentialId)
  if (query.model) params.model = query.model
  if (query.group) params.group = query.group
  if (query.onlyFailed) params.onlyFailed = 'true'
  if (query.sessionId) params.sessionId = query.sessionId
  if (query.onlySwitched) params.onlySwitched = 'true'
  if (query.clientIp) params.clientIp = query.clientIp
  if (query.startTime != null) params.startTime = String(query.startTime)
  if (query.endTime != null) params.endTime = String(query.endTime)
  if (query.q) params.q = query.q
  if (query.limit != null) params.limit = String(query.limit)
  if (query.offset != null) params.offset = String(query.offset)
  const { data } = await api.get<TracePage>('/traces', { params })
  return data
}

export async function getFailureStats(): Promise<FailureStatsMap> {
  const { data } = await api.get<FailureStatsMap>('/traces/failure-stats')
  return data
}

export interface ObservationConfig {
  enabled: boolean
  keyId: number | null
}

export interface ObservationSummary {
  id: string
  ts: string
  keyId: number
  path: string
  model: string | null
  labels: { name: string; heuristic: boolean }[]
  state: 'running' | 'complete' | 'error' | 'interrupted'
  statusCode: number | null
  contentType: string | null
  durationMs: number
  requestBytes: number
  requestTruncated: boolean
  responseBytes: number
  responseTruncated: boolean
}

export interface ObservationDetail extends ObservationSummary {
  requestBody: string
  responseBody: string
}

export async function getObservationConfig(): Promise<ObservationConfig> {
  return (await api.get<ObservationConfig>('/config/request-observation')).data
}

export async function setObservationConfig(config: ObservationConfig): Promise<ObservationConfig> {
  return (await api.put<ObservationConfig>('/config/request-observation', config)).data
}

export async function getObservations(keyId: number | undefined, offset: number) {
  return (await api.get<{ records: ObservationSummary[]; total: number }>('/observations', {
    params: { keyId, limit: 25, offset },
  })).data
}

export async function getObservation(id: string): Promise<ObservationDetail> {
  return (await api.get<ObservationDetail>(`/observations/${encodeURIComponent(id)}`)).data
}
