import { useMemo, useState } from 'react'
import { toast } from 'sonner'
import {
  Activity,
  AlertCircle,
  CheckCircle2,
  CircleDashed,
  Clock3,
  Loader2,
  RefreshCw,
  Server,
  XCircle,
} from 'lucide-react'
import { useCredentials } from '@/hooks/use-credentials'
import {
  checkSingleUpstreamHealth,
  checkUpstreamHealth,
  type UpstreamHealthResult,
} from '@/api/upstream-health'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import type { CredentialStatusItem } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

type ResultMap = Record<number, UpstreamHealthResult>

function isUpstreamCredential(credential: CredentialStatusItem): boolean {
  return Boolean(
    credential.isUpstream ||
      credential.authMethod === 'upstream' ||
      credential.provider === 'upstream' ||
      credential.provider === 'anthropic',
  )
}

function formatLatency(latencyMs: number | undefined): string {
  if (latencyMs == null || !Number.isFinite(latencyMs)) return '—'
  return `${Math.max(0, Math.round(latencyMs))} ms`
}

function formatCheckedAt(value: string | null): string {
  if (!value) return '尚未检测'
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString('zh-CN')
}

function credentialLabel(credential: CredentialStatusItem): string {
  return credential.email?.trim() || `凭据 #${credential.id}`
}

function StatusBadge({ result }: { result: UpstreamHealthResult | undefined }) {
  if (!result) {
    return (
      <Badge variant="outline" className="gap-1 text-muted-foreground">
        <CircleDashed className="h-3 w-3" />
        未检测
      </Badge>
    )
  }
  if (result.success) {
    return (
      <Badge variant="success" className="gap-1">
        <CheckCircle2 className="h-3 w-3" />
        可用
      </Badge>
    )
  }
  return (
    <Badge variant="destructive" className="gap-1">
      <XCircle className="h-3 w-3" />
      不可用
    </Badge>
  )
}

export function UpstreamHealthPage() {
  const { data: credentialsData, isLoading: credentialsLoading } = useCredentials()
  const [results, setResults] = useState<ResultMap>({})
  const [checkedAt, setCheckedAt] = useState<string | null>(null)
  const [checkingAll, setCheckingAll] = useState(false)
  const [checkingIds, setCheckingIds] = useState<Set<number>>(() => new Set())

  const upstreamCredentials = useMemo(
    () => (credentialsData?.credentials ?? []).filter(isUpstreamCredential),
    [credentialsData?.credentials],
  )

  const setChecking = (id: number, value: boolean) => {
    setCheckingIds((current) => {
      const next = new Set(current)
      if (value) next.add(id)
      else next.delete(id)
      return next
    })
  }

  const applyResults = (nextResults: UpstreamHealthResult[], nextCheckedAt?: string) => {
    setResults((current) => {
      const next = { ...current }
      nextResults.forEach((result) => {
        next[result.credentialId] = result
      })
      return next
    })
    if (nextCheckedAt) setCheckedAt(nextCheckedAt)
  }

  const checkAll = async () => {
    if (checkingAll || upstreamCredentials.length === 0) return
    setCheckingAll(true)
    try {
      const response = await checkUpstreamHealth(upstreamCredentials.map((credential) => credential.id))
      const checkedTimes = response.results
        .map((result) => result.checkedAt)
        .filter((value): value is string => Boolean(value))
        .sort()
      const resultCheckedAt = checkedTimes[checkedTimes.length - 1]
      applyResults(response.results, response.checkedAt ?? resultCheckedAt ?? new Date().toISOString())
      const failed = response.results.filter((result) => !result.success).length
      if (failed === 0) toast.success(`检测完成：${response.results.length} 个上游均可用`)
      else toast.warning(`检测完成：${failed} 个上游不可用`)
    } catch (error) {
      toast.error(`上游健康检查失败：${extractErrorMessage(error)}`)
    } finally {
      setCheckingAll(false)
    }
  }

  const checkOne = async (credentialId: number) => {
    if (checkingAll || checkingIds.has(credentialId)) return
    setChecking(credentialId, true)
    try {
      const result = await checkSingleUpstreamHealth(credentialId)
      applyResults([result], new Date().toISOString())
      if (result.success) toast.success(`凭据 #${credentialId} 可用，延迟 ${formatLatency(result.latencyMs)}`)
      else toast.error(`凭据 #${credentialId} 不可用${result.error ? `：${result.error}` : ''}`)
    } catch (error) {
      const message = extractErrorMessage(error)
      applyResults([
        {
          credentialId,
          success: false,
          latencyMs: 0,
          error: message,
        },
      ], new Date().toISOString())
      toast.error(`凭据 #${credentialId} 检查失败：${message}`)
    } finally {
      setChecking(credentialId, false)
    }
  }

  const summary = useMemo(() => {
    const rows = upstreamCredentials
      .map((credential) => results[credential.id])
      .filter((result): result is UpstreamHealthResult => Boolean(result))
    const healthy = rows.filter((result) => result.success).length
    const failed = rows.filter((result) => !result.success).length
    const latencyRows = rows.filter((result) => Number.isFinite(result.latencyMs) && result.latencyMs >= 0)
    const avgLatency = latencyRows.length
      ? latencyRows.reduce((total, result) => total + result.latencyMs, 0) / latencyRows.length
      : undefined
    return { checked: rows.length, healthy, failed, avgLatency }
  }, [results, upstreamCredentials])

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-[25px] font-semibold tracking-tight leading-tight">
            <Activity className="h-5 w-5 text-primary" />
            上游健康
          </h1>
          <p className="mt-1 text-sm text-muted-foreground">
            直接探测各凭据的 <code className="rounded bg-secondary px-1 py-0.5 text-[11px]">/v1/models</code>；延迟为收到响应头的时间，不会改变故障转移状态。
          </p>
        </div>
        <Button
          size="sm"
          onClick={checkAll}
          disabled={checkingAll || credentialsLoading || upstreamCredentials.length === 0}
          title="检测全部上游凭据"
        >
          {checkingAll ? <Loader2 className="animate-spin" /> : <RefreshCw />}
          {checkingAll ? '检测中…' : '检测全部'}
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-4">
        <SummaryCard icon={<Server className="h-4 w-4" />} label="上游凭据" value={upstreamCredentials.length} />
        <SummaryCard icon={<CheckCircle2 className="h-4 w-4" />} label="可用" value={summary.healthy} tone="success" />
        <SummaryCard icon={<AlertCircle className="h-4 w-4" />} label="不可用" value={summary.failed} tone="danger" />
        <SummaryCard
          icon={<Clock3 className="h-4 w-4" />}
          label="平均延迟"
          value={summary.avgLatency == null ? '—' : formatLatency(summary.avgLatency)}
        />
      </div>

      {checkedAt && (
        <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Clock3 className="h-3.5 w-3.5" />
          最近检查：{formatCheckedAt(checkedAt)} · 已完成 {summary.checked}/{upstreamCredentials.length}
        </div>
      )}

      {credentialsLoading ? (
        <Card>
          <CardContent className="py-14 text-center text-sm text-muted-foreground">加载凭据中…</CardContent>
        </Card>
      ) : upstreamCredentials.length === 0 ? (
        <Card>
          <CardContent className="py-14 text-center">
            <Server className="mx-auto mb-3 h-8 w-8 text-muted-foreground/50" />
            <p className="text-sm text-muted-foreground">暂无上游 Anthropic API 凭据</p>
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="overflow-x-auto p-0">
            <table className="w-full min-w-[760px] text-sm">
              <thead className="border-b border-border/60 text-[12px] text-muted-foreground">
                <tr className="whitespace-nowrap">
                  <th className="px-4 py-3 text-left font-medium">凭据</th>
                  <th className="px-4 py-3 text-left font-medium">地址</th>
                  <th className="px-4 py-3 text-left font-medium">状态</th>
                  <th className="px-4 py-3 text-right font-medium">延迟</th>
                  <th className="px-4 py-3 text-left font-medium">HTTP</th>
                  <th className="px-4 py-3 text-left font-medium">错误</th>
                  <th className="px-4 py-3 text-right font-medium">操作</th>
                </tr>
              </thead>
              <tbody>
                {upstreamCredentials.map((credential) => {
                  const result = results[credential.id]
                  const checking = checkingAll || checkingIds.has(credential.id)
                  return (
                    <tr key={credential.id} className="border-b border-border/50 last:border-0">
                      <td className="max-w-[230px] px-4 py-3 align-top">
                        <div className="truncate font-medium" title={credentialLabel(credential)}>
                          {credentialLabel(credential)}
                        </div>
                        <div className="mt-0.5 text-xs text-muted-foreground">
                          #{credential.id}{credential.disabled ? ' · 已禁用' : ''}
                        </div>
                      </td>
                      <td className="max-w-[250px] px-4 py-3 align-top">
                        <span className="block truncate text-xs text-muted-foreground" title={credential.upstreamBaseUrl || credential.endpoint}>
                          {credential.upstreamBaseUrl || credential.endpoint || '—'}
                        </span>
                      </td>
                      <td className="px-4 py-3 align-top"><StatusBadge result={result} /></td>
                      <td className="px-4 py-3 text-right align-top font-mono tabular-nums">
                        {checking ? <Loader2 className="ml-auto h-4 w-4 animate-spin text-muted-foreground" /> : formatLatency(result?.latencyMs)}
                      </td>
                      <td className="px-4 py-3 align-top font-mono text-xs tabular-nums">{result?.status ?? '—'}</td>
                      <td className="max-w-[300px] px-4 py-3 align-top">
                        <span className="block truncate text-xs text-muted-foreground" title={result?.error || undefined}>
                          {result?.error || '—'}
                        </span>
                      </td>
                      <td className="px-4 py-3 text-right align-top">
                        <Button
                          size="icon"
                          variant="ghost"
                          className="h-7 w-7"
                          onClick={() => checkOne(credential.id)}
                          disabled={checking}
                          title={`检测 ${credentialLabel(credential)}`}
                          aria-label={`检测 ${credentialLabel(credential)}`}
                        >
                          {checking ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                        </Button>
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}
    </div>
  )
}

function SummaryCard({
  icon,
  label,
  value,
  tone = 'default',
}: {
  icon: React.ReactNode
  label: string
  value: number | string
  tone?: 'default' | 'success' | 'danger'
}) {
  const toneClass = tone === 'success'
    ? 'text-emerald-600 dark:text-emerald-400'
    : tone === 'danger'
      ? 'text-destructive'
      : 'text-foreground'
  return (
    <Card>
      <CardContent className="flex items-center gap-3 p-4">
        <div className={`rounded-xl bg-secondary p-2 ${toneClass}`}>{icon}</div>
        <div className="min-w-0">
          <div className="text-xs text-muted-foreground">{label}</div>
          <div className={`mt-0.5 text-lg font-semibold tabular-nums ${toneClass}`}>{value}</div>
        </div>
      </CardContent>
    </Card>
  )
}
