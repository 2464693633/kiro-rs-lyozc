import { useMemo, useState } from 'react'
import { toast } from 'sonner'
import {
  Activity, Globe, Loader2, Network, Pencil, Play, Plus, RefreshCw, Search, Shuffle, Trash2, Users, X,
} from 'lucide-react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Button } from '@/components/ui/button'
import { Switch } from '@/components/ui/switch'
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { ConsoleTable, type ConsoleColumn } from '@/components/console/data-table'
import { PageHeader } from '@/components/console/page-header'
import { StatusStrip, type StatusSegment } from '@/components/console/status-strip'
import type { RailTone } from '@/components/console/rail'
import {
  assignProxiesRoundRobin, checkAllProxies, checkProxy, deleteProxy, getProxyCredentials,
  getProxyPool, setProxyAsGlobal, setProxyEnabled,
} from '@/api/credentials'
import { useGlobalProxy } from '@/hooks/use-credentials'
import { cn, extractErrorMessage, maskProxyUrl } from '@/lib/utils'
import { PROXY_PROTOCOLS, ProxyHealthBadge, formatLocation } from '@/components/proxy-format'
import { CreateProxyDialog, EditProxyDialog } from '@/components/proxy-form-dialog'
import type { ProxyPoolEntry } from '@/types/api'

type StatusFilter = 'all' | 'healthy' | 'unhealthy' | 'unknown' | 'disabled'

/** 已禁用优先；否则按最近一次检测结果 */
function statusOf(p: ProxyPoolEntry): Exclude<StatusFilter, 'all'> {
  return p.enabled ? p.health : 'disabled'
}

/** 行左侧色轨：检测失败或被自动禁用标红，手动禁用不标 */
function rowTone(p: ProxyPoolEntry): RailTone {
  if (p.autoDisabled || (p.enabled && p.health === 'unhealthy')) return 'dead'
  if (p.enabled && p.health === 'healthy') return 'ok'
  return 'none'
}

function matchesQuery(p: ProxyPoolEntry, q: string): boolean {
  if (!q) return true
  return [p.name, p.host, p.username, p.exit?.ip, p.exit?.city, p.exit?.country]
    .some((v) => v?.toLowerCase().includes(q))
}

function formatTime(iso?: string): string {
  return iso ? new Date(iso).toLocaleString('zh-CN', { hour12: false }) : ''
}

/**
 * IP 管理：保存代理后在添加/编辑账号时直接选择。
 *
 * - 账号保存的是代理的配置副本，编辑代理会同步到所有关联账号
 * - 仍有账号关联的代理不能删除，需先给这些账号换代理
 * - 禁用只影响新的选择与轮询分配，已关联的账号继续使用
 */
export function ProxiesPage() {
  const queryClient = useQueryClient()
  const confirm = useConfirm()
  const { data, isLoading, isFetching, refetch } = useQuery({ queryKey: ['proxy-pool'], queryFn: getProxyPool })
  const { data: globalProxy } = useGlobalProxy()

  const [query, setQuery] = useState('')
  const [protocol, setProtocol] = useState('all')
  const [status, setStatus] = useState<StatusFilter>('all')
  const [createOpen, setCreateOpen] = useState(false)
  const [editing, setEditing] = useState<ProxyPoolEntry | null>(null)
  const [viewing, setViewing] = useState<ProxyPoolEntry | null>(null)
  const [testing, setTesting] = useState<Set<number>>(new Set())

  const proxies = useMemo(() => data?.proxies ?? [], [data])
  const counts = useMemo(() => {
    const c = { all: proxies.length, healthy: 0, unhealthy: 0, unknown: 0, disabled: 0 }
    for (const p of proxies) c[statusOf(p)]++
    return c
  }, [proxies])
  const rows = useMemo(() => {
    const q = query.trim().toLowerCase()
    return proxies.filter(
      (p) =>
        (protocol === 'all' || p.protocol === protocol) &&
        (status === 'all' || statusOf(p) === status) &&
        matchesQuery(p, q),
    )
  }, [proxies, query, protocol, status])

  /** 地址与用户名只用于标记全局代理；密码不回显，仍需允许重新应用认证。 */
  const isGlobal = (p: ProxyPoolEntry) =>
    Boolean(globalProxy?.proxyUrl) &&
    globalProxy?.proxyUrl === p.url &&
    (globalProxy?.proxyUsername ?? '') === (p.username ?? '')
  const refreshPool = () => queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })

  const testOne = useMutation({
    mutationFn: (p: ProxyPoolEntry) => checkProxy(p.id),
    onMutate: (p) => setTesting((s) => new Set(s).add(p.id)),
    onSuccess: (entry) => {
      if (entry.health === 'healthy') {
        const where = formatLocation(entry.exit) || entry.exit?.ip
        toast.success(`${entry.name} 可用，延迟 ${entry.latencyMs ?? '-'} ms${where ? `，出口 ${where}` : ''}`)
      } else {
        toast.error(`${entry.name} 检测失败${entry.lastError ? `：${entry.lastError}` : ''}`)
      }
    },
    onError: (err) => toast.error(`检测失败: ${extractErrorMessage(err)}`),
    onSettled: (_data, _err, p) => {
      setTesting((s) => {
        const next = new Set(s)
        next.delete(p.id)
        return next
      })
      refreshPool()
    },
  })

  const testAll = useMutation({
    mutationFn: () => checkAllProxies(),
    onSuccess: (res) => {
      const disabled = res.autoDisabled > 0 ? `，${res.autoDisabled} 个连续失败被自动禁用` : ''
      toast.success(`检测完成：可用 ${res.healthy}，异常 ${res.unhealthy}${disabled}`)
    },
    onError: (err) => toast.error(`检测失败: ${extractErrorMessage(err)}`),
    onSettled: refreshPool,
  })

  const toggle = useMutation({
    mutationFn: (p: ProxyPoolEntry) => setProxyEnabled(p.id, !p.enabled),
    onSuccess: (_res, p) => {
      if (p.enabled) {
        toast.success(
          p.credentialCount > 0
            ? `已禁用 ${p.name}；已关联的 ${p.credentialCount} 个账号继续使用它`
            : `已禁用 ${p.name}`,
        )
      } else {
        toast.success(`已启用 ${p.name}`)
      }
      refreshPool()
    },
    onError: (err) => toast.error(`操作失败: ${extractErrorMessage(err)}`),
  })

  const setGlobal = useMutation({
    mutationFn: (p: ProxyPoolEntry) => setProxyAsGlobal(p.id),
    onSuccess: (_res, p) => {
      toast.success(`已将 ${p.name} 设为全局代理`)
      queryClient.invalidateQueries({ queryKey: ['global-proxy'] })
    },
    onError: (err) => toast.error(`设置失败: ${extractErrorMessage(err)}`),
  })

  const remove = useMutation({
    mutationFn: (p: ProxyPoolEntry) => deleteProxy(p.id),
    onSuccess: (_res, p) => {
      toast.success(`已删除 ${p.name}`)
      refreshPool()
    },
    onError: (err) => toast.error(`删除失败: ${extractErrorMessage(err)}`),
  })

  const roundRobin = useMutation({
    mutationFn: () => assignProxiesRoundRobin(null),
    onSuccess: (res) => {
      toast.success(`已用 ${res.proxyCount} 个代理轮询分配给 ${res.assigned} 个账号`)
      refreshPool()
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (err) => toast.error(`分配失败: ${extractErrorMessage(err)}`),
  })

  const handleSetGlobal = async (p: ProxyPoolEntry) => {
    const ok = await confirm({
      title: `将 ${p.name} 设为全局代理？`,
      description: `所有未单独指定代理的账号都会改走 ${p.url}。${
        p.health === 'unhealthy' ? '注意：它最近一次检测失败。' : ''
      }`,
      confirmText: '设为全局',
    })
    if (ok) setGlobal.mutate(p)
  }

  const handleDelete = async (p: ProxyPoolEntry) => {
    // 仍有账号使用时后端会拒绝，直接列出这些账号
    if (p.credentialCount > 0) {
      setViewing(p)
      return
    }
    const ok = await confirm({
      title: `删除代理 ${p.name}？`,
      description: `${p.url} 将从 IP 管理中移除，此操作不可撤销。`,
      confirmText: '删除',
      destructive: true,
    })
    if (ok) remove.mutate(p)
  }

  const handleRoundRobin = async () => {
    const usable = proxies.filter((p) => p.enabled && p.health !== 'unhealthy').length
    if (usable === 0) {
      toast.error('没有可用代理（需已启用且最近一次检测未失败）')
      return
    }
    const ok = await confirm({
      title: '轮询分配代理？',
      description: `把 ${usable} 个可用代理依次分配给全部账号，会覆盖每个账号当前的代理设置（包括使用全局代理和直连的账号）。`,
      confirmText: '分配',
      destructive: true,
    })
    if (ok) roundRobin.mutate()
  }

  const columns: ConsoleColumn<ProxyPoolEntry>[] = [
    {
      id: 'name',
      header: '名称',
      cell: (p) => (
        <div className="min-w-0">
          <div className="flex items-center gap-1.5">
            <span className="max-w-[200px] truncate font-medium text-foreground" title={p.name}>{p.name}</span>
            {isGlobal(p) && (
              <span className="shrink-0 rounded bg-primary/10 px-1.5 text-[11px] text-primary">全局</span>
            )}
          </div>
          <div className="console-num text-[11px] text-muted-foreground">#{p.id}</div>
        </div>
      ),
    },
    {
      id: 'protocol',
      header: '协议',
      cell: (p) => <span className="console-num text-[12px] uppercase">{p.protocol}</span>,
    },
    {
      id: 'address',
      header: '地址',
      cell: (p) => <span className="console-num text-[12px]">{p.url.replace(/^[a-z0-9]+:\/\//, '')}</span>,
    },
    {
      id: 'auth',
      header: '认证',
      cell: (p) =>
        p.username ? (
          <span className="console-num text-[12px]" title={p.hasPassword ? '已设置密码' : '未设置密码'}>
            {p.username}
            <span className="text-muted-foreground">{p.hasPassword ? ' · ******' : ''}</span>
          </span>
        ) : (
          <span className="text-muted-foreground">—</span>
        ),
    },
    {
      id: 'exit',
      header: '出口',
      hint: '最近一次检测到的出口 IP 与位置',
      cell: (p) => <ExitCell proxy={p} />,
    },
    {
      id: 'accounts',
      header: '账号',
      hint: '使用此代理的账号数，点击查看',
      cell: (p) => (
        <button
          type="button"
          onClick={() => setViewing(p)}
          className="inline-flex items-center gap-1 rounded-full bg-secondary px-2 py-0.5 text-[11px] font-medium tabular-nums text-secondary-foreground transition-colors hover:bg-accent"
          title="查看使用此代理的账号"
        >
          <Users className="h-3 w-3 text-muted-foreground" />
          {p.credentialCount}
        </button>
      ),
    },
    {
      id: 'health',
      header: '连通性',
      cell: (p) => <HealthCell proxy={p} />,
    },
    {
      id: 'enabled',
      header: '启用',
      cell: (p) => (
        <div className="flex items-center gap-2">
          <Switch
            checked={p.enabled}
            onCheckedChange={() => toggle.mutate(p)}
            disabled={toggle.isPending && toggle.variables?.id === p.id}
            aria-label={p.enabled ? '禁用此代理' : '启用此代理'}
          />
          {p.autoDisabled && (
            <span
              className="text-[11px] text-destructive"
              title="连续检测失败后被自动禁用；重新检测通过后需手动启用"
            >
              自动禁用
            </span>
          )}
        </div>
      ),
    },
  ]

  const rowActions = (p: ProxyPoolEntry) => (
    <>
      <Button size="icon" variant="ghost" onClick={() => testOne.mutate(p)} disabled={testing.has(p.id)} title="检测连通性">
        {testing.has(p.id) ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Play className="h-3.5 w-3.5" />}
      </Button>
      <Button size="icon" variant="ghost" onClick={() => setEditing(p)} title="编辑">
        <Pencil className="h-3.5 w-3.5" />
      </Button>
      <Button
        size="icon"
        variant="ghost"
        onClick={() => handleSetGlobal(p)}
        disabled={!p.enabled || setGlobal.isPending}
        title={!p.enabled ? '已禁用的代理不能设为全局代理' : isGlobal(p) ? '重新应用全局代理（同步认证）' : '设为全局代理'}
      >
        <Globe className="h-3.5 w-3.5" />
      </Button>
      <Button
        size="icon"
        variant="ghost"
        className="text-destructive hover:text-destructive"
        onClick={() => handleDelete(p)}
        disabled={remove.isPending}
        title={p.credentialCount > 0 ? `仍有 ${p.credentialCount} 个账号在使用，不能删除` : '删除'}
      >
        <Trash2 className="h-3.5 w-3.5" />
      </Button>
    </>
  )

  const segments: StatusSegment[] = (
    [
      ['all', '全部', 'none'],
      ['healthy', '可用', 'ok'],
      ['unhealthy', '异常', 'dead'],
      ['unknown', '未检测', 'none'],
      ['disabled', '已禁用', 'warn'],
    ] as const
  ).map(([key, label, tone]) => ({
    label,
    tone,
    count: counts[key],
    active: status === key,
    onClick: () => setStatus(key),
  }))

  return (
    <div className="console-scope space-y-4">
      <PageHeader
        breadcrumbs={[{ label: '控制台' }, { label: 'IP 管理', active: true }]}
        icon={<Network className="h-4 w-4" />}
        title="IP 管理"
        description="先在这里保存代理，添加或编辑账号时直接选择。编辑代理会同步到所有关联账号；仍有账号在用的代理不能删除。"
        actions={
          <>
            <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isFetching}>
              <RefreshCw className={cn('h-3.5 w-3.5', isFetching && 'animate-spin')} />
              刷新
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={() => testAll.mutate()}
              disabled={testAll.isPending || counts.all === 0}
              title="检测所有已启用的代理"
            >
              {testAll.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Activity className="h-3.5 w-3.5" />}
              全部检测
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={handleRoundRobin}
              disabled={roundRobin.isPending || counts.all === 0}
              title="把可用代理依次分配给全部账号"
            >
              <Shuffle className="h-3.5 w-3.5" />
              轮询分配
            </Button>
            <Button size="sm" onClick={() => setCreateOpen(true)}>
              <Plus className="h-3.5 w-3.5" />
              添加代理
            </Button>
          </>
        }
      />

      <GlobalProxyNote url={globalProxy?.proxyUrl ?? null} />

      <div className="flex flex-wrap items-center justify-between gap-2">
        <StatusStrip segments={segments} />
        <div className="flex w-full items-center gap-2 sm:w-auto">
          <div className="relative min-w-0 flex-1 sm:w-[220px] sm:flex-none">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="搜索名称 / 主机 / 出口"
              className="h-8 w-full rounded-md border border-border bg-card pl-8 pr-7 text-xs placeholder:text-muted-foreground/60 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
            />
            {query && (
              <button
                type="button"
                onClick={() => setQuery('')}
                className="absolute right-2 top-1/2 flex h-4 w-4 -translate-y-1/2 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground"
                title="清除搜索"
              >
                <X className="h-3 w-3" />
              </button>
            )}
          </div>
          <div className="w-[120px] shrink-0">
            <Select value={protocol} onValueChange={setProtocol}>
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">全部协议</SelectItem>
                {PROXY_PROTOCOLS.map((p) => (
                  <SelectItem key={p.value} value={p.value}>{p.label}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </div>
      </div>

      <ConsoleTable
        rows={rows}
        columns={columns}
        rowKey={(p) => p.id}
        tone={rowTone}
        rowActions={rowActions}
        loading={isLoading}
        empty={counts.all === 0 ? '还没有代理。点击右上角「添加代理」，可单个填写或批量粘贴。' : '没有匹配的代理'}
      />

      {createOpen && <CreateProxyDialog open={createOpen} onOpenChange={setCreateOpen} existing={proxies} />}
      {editing && (
        <EditProxyDialog
          proxy={editing}
          isGlobal={isGlobal(editing)}
          onOpenChange={(open) => {
            if (!open) setEditing(null)
          }}
        />
      )}
      {viewing && <LinkedAccountsDialog proxy={viewing} onClose={() => setViewing(null)} />}
    </div>
  )
}

/** 全局代理：未单独指定代理的账号走它 */
function GlobalProxyNote({ url }: { url: string | null }) {
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-lg border border-border/60 bg-card px-3 py-2 text-xs text-muted-foreground">
      <Globe className="h-3.5 w-3.5 shrink-0" />
      <span>全局代理（未单独指定代理的账号使用）：</span>
      <span className="console-num text-foreground">{url ? maskProxyUrl(url) : '未配置，直连'}</span>
      <a href="#/settings?s=network" className="text-primary hover:underline">修改</a>
    </div>
  )
}

/** 使用某个代理的账号；删除被拒时也用它说明原因 */
function LinkedAccountsDialog({ proxy, onClose }: { proxy: ProxyPoolEntry; onClose: () => void }) {
  const { data, isLoading } = useQuery({
    queryKey: ['proxy-credentials', proxy.id],
    queryFn: () => getProxyCredentials(proxy.id),
  })
  const items = data ?? []
  return (
    <Dialog open onOpenChange={(open) => { if (!open) onClose() }}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>使用 {proxy.name} 的账号</DialogTitle>
          <DialogDescription>
            编辑代理会同步到这些账号。要删除此代理，先在凭据管理里给它们换代理。
          </DialogDescription>
        </DialogHeader>
        <div className="max-h-72 overflow-y-auto rounded-md border">
          {isLoading && <div className="p-4 text-center text-xs text-muted-foreground">加载中…</div>}
          {!isLoading && items.length === 0 && (
            <div className="p-4 text-center text-xs text-muted-foreground">没有账号使用此代理</div>
          )}
          {items.map((c) => (
            <div key={c.id} className="flex items-center gap-2 border-b px-3 py-2 text-xs last:border-b-0">
              <span className="font-mono text-muted-foreground">#{c.id}</span>
              <span className="min-w-0 flex-1 truncate">{c.email || '（未设置邮箱）'}</span>
              {c.disabled && <span className="shrink-0 text-[11px] text-muted-foreground">已禁用</span>}
            </div>
          ))}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>关闭</Button>
          {items.length > 0 && (
            <Button onClick={() => { window.location.hash = '#/credentials' }}>去凭据管理</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function HealthCell({ proxy }: { proxy: ProxyPoolEntry }) {
  const failed = proxy.health === 'unhealthy' && proxy.lastError
  return (
    <div className="space-y-0.5">
      <ProxyHealthBadge proxy={proxy} />
      {failed ? (
        <div className="max-w-[220px] truncate text-[11px] text-destructive" title={proxy.lastError}>
          {proxy.lastError}
        </div>
      ) : proxy.lastCheckedAt ? (
        <div className="console-num text-[11px] text-muted-foreground">{formatTime(proxy.lastCheckedAt)}</div>
      ) : null}
    </div>
  )
}

function ExitCell({ proxy }: { proxy: ProxyPoolEntry }) {
  const exit = proxy.exit
  if (!exit) return <span className="text-muted-foreground">—</span>
  const where = formatLocation(exit)
  const full = [exit.city, exit.region, exit.country].filter(Boolean).join(' / ')
  return (
    <div className="min-w-0">
      {where && <div className="max-w-[180px] truncate text-[12px]" title={full}>{where}</div>}
      <div className="console-num text-[11px] text-muted-foreground">{exit.ip}</div>
    </div>
  )
}
