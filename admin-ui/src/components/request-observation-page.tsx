import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Copy, Download, RefreshCw, ScanText } from 'lucide-react'
import { toast } from 'sonner'
import {
  getObservation, getObservationConfig, getObservations, setObservationConfig,
  type ObservationConfig, type ObservationSummary,
} from '@/api/traces'
import { useClientKeys } from '@/hooks/use-client-keys'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { PageHeader } from '@/components/console/page-header'
import { ConsoleTable, type ConsoleColumn } from '@/components/console/data-table'

const FEATURE_NAMES: Record<string, string> = {
  streaming: '流式', non_streaming: '非流式', thinking: '思考模式',
  structured_output: '结构化输出', tool_choice: '工具选择', tools: '工具声明',
  web_search: '网页搜索', image: '图片', document: '文档', thinking_replay: '思考回传',
  signature_replay: '签名回传', cache_control: '缓存控制', strict_tool: '严格工具',
  number_fingerprint_candidate: '数字指纹', knowledge_probe_candidate: '知识截止探测',
  identity_probe_candidate: '模型身份探测',
}
const STATES = { running: '处理中', complete: '完成', error: '失败', interrupted: '中断' }

function Features({ labels }: Pick<ObservationSummary, 'labels'>) {
  return <div className="flex max-w-md flex-wrap gap-1">{labels.length ? labels.map((label) => (
    <Badge key={label.name} variant={label.heuristic ? 'warning' : 'secondary'}>
      {FEATURE_NAMES[label.name] ?? label.name}{label.heuristic ? '（推测）' : ''}
    </Badge>
  )) : <span className="text-muted-foreground">未识别特征</span>}</div>
}

function pretty(body: string) {
  try { return JSON.stringify(JSON.parse(body), null, 2) }
  catch { return body }
}

function Detail({ id }: { id: string }) {
  const [tab, setTab] = useState<'request' | 'response'>('request')
  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: ['observation-detail', id], queryFn: () => getObservation(id),
    refetchInterval: (query) => query.state.data?.state === 'running' ? 2000 : false,
  })
  if (isLoading) return <div className="p-5 text-sm text-muted-foreground">正在读取内容…</div>
  if (isError || !data) return <div className="p-5 text-sm">读取失败或记录已过期。<Button variant="link" onClick={() => refetch()}>重试</Button></div>
  const request = tab === 'request'
  const body = pretty(request ? data.requestBody : data.responseBody)
  const truncated = request ? data.requestTruncated : data.responseTruncated
  const download = () => {
    const link = document.createElement('a')
    const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' }))
    link.href = url
    link.download = `observation-${id}.json`
    link.click()
    window.setTimeout(() => URL.revokeObjectURL(url), 0)
  }
  return <div className="space-y-3 bg-muted/20 p-4">
    <div className="flex flex-wrap items-center justify-between gap-2">
      <div className="flex gap-1">
        <Button size="sm" variant={request ? 'secondary' : 'ghost'} onClick={() => setTab('request')}>原始请求</Button>
        <Button size="sm" variant={request ? 'ghost' : 'secondary'} onClick={() => setTab('response')}>响应 / SSE</Button>
      </div>
      <div className="flex gap-2">
        <Button size="sm" variant="outline" onClick={async () => {
          try { await navigator.clipboard.writeText(body); toast.success('已复制内容') }
          catch { toast.error('复制失败，请手动选择文本') }
        }}><Copy className="mr-1 h-3.5 w-3.5" />复制</Button>
        <Button size="sm" variant="outline" onClick={download}><Download className="mr-1 h-3.5 w-3.5" />导出</Button>
      </div>
    </div>
    <div className="flex flex-wrap gap-3 text-xs text-muted-foreground">
      <span>{data.path}</span><span>HTTP {data.statusCode ?? '等待响应'}</span>
      <span>{data.contentType ?? '—'}</span><span>{STATES[data.state]}</span>
      <span>请求 {data.requestBytes.toLocaleString()} B / 响应 {data.responseBytes.toLocaleString()} B</span>
    </div>
    {truncated && <p className="text-xs text-amber-600 dark:text-amber-400">正文超过 256 KiB，仅保存前缀；实际转发内容保持完整。</p>}
    {!request && data.state === 'running' && <p className="text-xs text-muted-foreground">请求仍在执行，响应正文结束或断开后保存。此处会自动刷新。</p>}
    <pre className="max-h-[32rem] overflow-auto whitespace-pre-wrap break-all rounded-md border bg-background p-4 font-mono text-xs leading-5">{body || '暂无正文'}</pre>
  </div>
}

export function RequestObservationPage() {
  const client = useQueryClient()
  const [page, setPage] = useState(0)
  const [keyFilter, setKeyFilter] = useState('')
  const { data: keys } = useClientKeys()
  const config = useQuery({ queryKey: ['observation-config'], queryFn: getObservationConfig })
  const save = useMutation({
    mutationFn: setObservationConfig,
    onSuccess: (value) => { client.setQueryData(['observation-config'], value); toast.success(value.enabled ? '内容观察已开启' : '观察配置已保存') },
    onError: () => toast.error('保存失败，请重试'),
  })
  const list = useQuery({
    queryKey: ['observations', keyFilter, page],
    queryFn: () => getObservations(keyFilter ? Number(keyFilter) : undefined, page * 25),
    refetchInterval: 5000,
  })
  const update = (patch: Partial<ObservationConfig>) => {
    if (config.data) save.mutate({ ...config.data, ...patch })
  }
  const names = new Map((keys?.keys ?? []).map((key) => [key.id, key.name]))
  const columns: ConsoleColumn<ObservationSummary>[] = [
    { id: 'time', header: '时间', cell: (row) => <span className="whitespace-nowrap">{new Date(row.ts).toLocaleString('zh-CN', { hour12: false })}</span> },
    { id: 'key', header: '客户端 Key', cell: (row) => names.get(row.keyId) ?? `#${row.keyId}` },
    { id: 'model', header: '模型 / 接口', cell: (row) => <div><div>{row.model ?? '未解析模型'}</div><div className="mt-1 text-[11px] text-muted-foreground">{row.path}</div></div> },
    { id: 'features', header: '内容特征', cell: (row) => <Features labels={row.labels} /> },
    { id: 'status', header: '状态', cell: (row) => <div>{STATES[row.state]}<span className="ml-2 text-muted-foreground">{row.statusCode ?? '—'}</span></div> },
    { id: 'duration', header: '耗时', align: 'right', cell: (row) => row.state === 'running' ? '—' : `${(row.durationMs / 1000).toFixed(2)}s` },
  ]
  const selectClass = 'h-9 max-w-full rounded-md border border-input bg-background px-3 text-sm'
  return <div className="console-scope space-y-4">
    <PageHeader breadcrumbs={[{ label: '控制台' }, { label: '请求日志' }, { label: '内容观察', active: true }]}
      icon={<ScanText className="h-4 w-4" />} title="内容观察"
      description="查看指定 Key 收到的真实请求、内容特征与对应响应，排查 API 兼容性。" />
    <section className="space-y-3 rounded-lg border bg-card p-4" aria-label="内容观察设置">
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div>
          <label htmlFor="observation-key" className="text-sm font-medium">检测专用 Key</label>
          <p className="mt-1 text-xs text-muted-foreground">在检测网站填写同一把 Key，即可将本轮检测与普通调用区分。</p>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          <select id="observation-key" className={selectClass} value={config.data?.keyId ?? ''}
            disabled={!config.data || save.isPending} onChange={(event) => update({ keyId: event.target.value ? Number(event.target.value) : null, enabled: false })}>
            <option value="">请选择客户端 Key</option>
            {keys?.keys.filter((key) => key.id > 0).map((key) => <option key={key.id} value={key.id}>{key.name}{key.disabled ? '（已停用）' : ''}</option>)}
          </select>
          <label className="flex items-center gap-2 text-sm">
            <Switch aria-label="开启内容观察" checked={config.data?.enabled ?? false}
              disabled={!config.data?.keyId || save.isPending} onCheckedChange={(enabled) => update({ enabled })} />
            {config.data?.enabled ? '观察中' : '已关闭'}
          </label>
        </div>
      </div>
      <p className="text-xs leading-5 text-muted-foreground">开启后保存所选 Key 的请求及响应正文，不记录鉴权头。记录保留 24 小时、最多 500 条；每侧正文最多 256 KiB。标有“推测”的标签仅描述内容特征，不代表已确认请求来自某个检测网站。</p>
      {config.isError && <p className="text-sm text-destructive">配置读取失败。<Button variant="link" onClick={() => config.refetch()}>重试</Button></p>}
    </section>
    <div className="flex flex-wrap items-center justify-between gap-2">
      <select aria-label="筛选观察记录的 Key" className={selectClass} value={keyFilter}
        onChange={(event) => { setKeyFilter(event.target.value); setPage(0) }}>
        <option value="">全部已观察 Key</option>
        {keys?.keys.map((key) => <option key={key.id} value={key.id}>{key.name}</option>)}
      </select>
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <span>{list.data?.total ?? 0} 条 · 每 5 秒刷新</span>
        <Button size="sm" variant="outline" aria-label="刷新观察记录" onClick={() => list.refetch()} disabled={list.isFetching}>
          <RefreshCw className={`h-3.5 w-3.5 ${list.isFetching ? 'animate-spin' : ''}`} />
        </Button>
      </div>
    </div>
    {list.isError && <p className="text-sm text-destructive">记录读取失败，请刷新重试。</p>}
    <ConsoleTable rows={list.data?.records ?? []} columns={columns} rowKey={(row) => row.id}
      loading={list.isLoading} renderExpandedRow={(row) => <Detail id={row.id} />}
      empty="暂无观察记录。先选择检测专用 Key 并开启观察，再使用该 Key 发起一次检测。" />
    {(list.data?.total ?? 0) > 25 && <div className="flex justify-center gap-3">
      <Button size="sm" variant="outline" disabled={page === 0} onClick={() => setPage(page - 1)}>上一页</Button>
      <span className="self-center text-sm">第 {page + 1} 页</span>
      <Button size="sm" variant="outline" disabled={(page + 1) * 25 >= (list.data?.total ?? 0)} onClick={() => setPage(page + 1)}>下一页</Button>
    </div>}
  </div>
}
