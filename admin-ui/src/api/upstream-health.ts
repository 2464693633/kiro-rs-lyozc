import axios from 'axios'
import { storage } from '@/lib/storage'

/** 单个上游 Anthropic 凭据的即时探测结果。 */
export interface UpstreamHealthResult {
  credentialId: number
  success: boolean
  latencyMs: number
  status?: number
  error?: string
  /** 当前后端逐条返回的完成时间；批量契约也可能在响应顶层返回 checkedAt。 */
  checkedAt?: string
}

/** 批量上游健康检查响应。 */
export interface UpstreamHealthResponse {
  checkedAt?: string
  results: UpstreamHealthResult[]
  total?: number
  successCount?: number
  failureCount?: number
}

export interface UpstreamHealthRequest {
  credentialIds?: number[]
}

const api = axios.create({
  baseURL: '/api/admin',
  // 服务端最多同时探测 8 个凭据；大量挂起凭据会按批次等待 20 秒超时。
  timeout: 300_000,
  headers: {
    'Content-Type': 'application/json',
  },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

/** 检测所有上游凭据，或只检测指定的凭据。 */
export async function checkUpstreamHealth(
  credentialIds?: number[],
): Promise<UpstreamHealthResponse> {
  const body: UpstreamHealthRequest =
    credentialIds && credentialIds.length > 0 ? { credentialIds } : {}
  const { data } = await api.post<UpstreamHealthResponse>('/credentials/upstream-health', body)
  return data
}

/** 检测单个上游凭据，便于列表中单行重试。 */
export async function checkSingleUpstreamHealth(
  credentialId: number,
): Promise<UpstreamHealthResult> {
  const { data } = await api.post<UpstreamHealthResult>(
    `/credentials/${credentialId}/health`,
    undefined,
  )
  return data
}
