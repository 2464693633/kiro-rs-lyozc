import { memo, useMemo, useState } from 'react'
import { toast } from 'sonner'
import { Check, ChevronDown, Loader2, Play, Search } from 'lucide-react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import { checkAllProxies, checkProxy, getGlobalProxy, getProxyPool } from '@/api/credentials'
import { extractErrorMessage, maskProxyUrl, cn } from '@/lib/utils'
import { ProxyHealthBadge, formatLocation } from '@/components/proxy-format'
import { sameProxyChoice, type ProxyChoice } from '@/lib/credential-proxy'
export { proxyChoiceOf, proxyRequestFields, sameProxyChoice, type ProxyChoice } from '@/lib/credential-proxy'

interface ProxySelectorProps {
  value: ProxyChoice
  onChange: (value: ProxyChoice) => void
  disabled?: boolean
}

/** 账号的代理下拉：可搜索、显示健康状态与出口位置、逐个或全部测试 */
export const ProxySelector = memo(function ProxySelector({
  value,
  onChange,
  disabled,
}: ProxySelectorProps) {
  const [query, setQuery] = useState('')
  const [testingId, setTestingId] = useState<number | null>(null)
  const queryClient = useQueryClient()

  const { data: pool, isLoading } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
    staleTime: 30_000,
  })
  const { data: globalProxy } = useQuery({
    queryKey: ['global-proxy'],
    queryFn: getGlobalProxy,
    staleTime: 30_000,
  })

  const proxies = useMemo(() => pool?.proxies ?? [], [pool])
  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase()
    // 已禁用的代理不能新选，但当前选中的仍保留显示
    const selectable = proxies.filter(
      (p) => p.enabled || (value.kind === 'pool' && value.id === p.id),
    )
    if (!q) return selectable
    return selectable.filter(
      (p) => p.name.toLowerCase().includes(q) || p.host.toLowerCase().includes(q),
    )
  }, [proxies, query, value])

  const testOne = useMutation({
    mutationFn: (id: number) => checkProxy(id),
    onMutate: (id) => setTestingId(id),
    onSuccess: (entry) => {
      if (entry.health === 'healthy') {
        const where = formatLocation(entry.exit) || entry.exit?.ip || ''
        toast.success(`${entry.name} 可用，延迟 ${entry.latencyMs ?? '-'} ms${where ? `，出口 ${where}` : ''}`)
      } else {
        toast.error(`${entry.name} 探测失败${entry.lastError ? `：${entry.lastError}` : ''}`)
      }
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    },
    onError: (err) => toast.error(`探测失败: ${extractErrorMessage(err)}`),
    onSettled: () => setTestingId(null),
  })

  const testAll = useMutation({
    mutationFn: () => checkAllProxies(),
    onSuccess: (res) => {
      toast.success(`检测完成：可用 ${res.healthy}，异常 ${res.unhealthy}`)
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    },
    onError: (err) => toast.error(`检测失败: ${extractErrorMessage(err)}`),
  })

  const selected = value.kind === 'pool' ? proxies.find((p) => p.id === value.id) : undefined
  const triggerLabel = (() => {
    switch (value.kind) {
      case 'global':
        return globalProxy?.proxyUrl
          ? `全局代理（${maskProxyUrl(globalProxy.proxyUrl)}）`
          : '全局代理（未配置，直连）'
      case 'direct':
        return '直连（不使用代理）'
      case 'custom':
        return `自定义：${maskProxyUrl(value.url)}`
      case 'pool':
        if (!selected) return isLoading ? '加载中…' : `代理 #${value.id}（已不存在）`
        return `${selected.name}（${selected.url}）${selected.enabled ? '' : ' · 已禁用'}`
    }
  })()
  const choose = (next: ProxyChoice) => {
    onChange(next)
    setQuery('')
  }
  const isSelected = (choice: ProxyChoice) => sameProxyChoice(value, choice)
  const tick = (on: boolean) => (on ? <Check className="h-3.5 w-3.5 shrink-0 text-primary" /> : null)

  return (
    <DropdownMenu onOpenChange={(open) => !open && setQuery('')}>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant="outline"
          disabled={disabled}
          className="h-10 w-full justify-between rounded-xl px-3.5 font-normal"
        >
          <span className="truncate">{triggerLabel}</span>
          <ChevronDown className="h-4 w-4 shrink-0 opacity-50" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="start"
        className="p-0"
        style={{ width: 'max(var(--radix-dropdown-menu-trigger-width), 320px)' }}
      >
        <div className="flex items-center gap-2 border-b px-3 py-2">
          <Search className="h-3.5 w-3.5 text-muted-foreground" />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            // 打字时不触发菜单的按键导航；方向键与 Esc 仍交给菜单处理
            onKeyDown={(e) => {
              if (!['ArrowDown', 'ArrowUp', 'Escape', 'Tab'].includes(e.key)) e.stopPropagation()
            }}
            placeholder="搜索名称或主机"
            className="flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
          />
          {proxies.length > 0 && (
            <Button
              type="button"
              size="sm"
              variant="ghost"
              className="h-7 px-2 text-xs"
              disabled={testAll.isPending}
              onClick={() => testAll.mutate()}
              title="测试全部已启用的代理"
            >
              {testAll.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Play className="h-3.5 w-3.5" />}
              全部测试
            </Button>
          )}
        </div>
        <div className="max-h-72 overflow-y-auto p-1">
          <DropdownMenuItem onSelect={() => choose({ kind: 'global' })} className="cursor-pointer gap-2">
            <div className="min-w-0 flex-1">
              <div>使用全局代理</div>
              <div className="truncate text-xs text-muted-foreground">
                {globalProxy?.proxyUrl ? maskProxyUrl(globalProxy.proxyUrl) : '未配置全局代理（直连）'}
              </div>
            </div>
            {tick(isSelected({ kind: 'global' }))}
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={() => choose({ kind: 'direct' })} className="cursor-pointer gap-2">
            <span className="flex-1">直连（不使用代理）</span>
            {tick(isSelected({ kind: 'direct' }))}
          </DropdownMenuItem>
          {value.kind === 'custom' && (
            <DropdownMenuItem onSelect={() => choose(value)} className="cursor-pointer gap-2">
              <div className="min-w-0 flex-1">
                <div>保持当前自定义代理</div>
                <div className="truncate text-xs text-muted-foreground">{maskProxyUrl(value.url)}</div>
              </div>
              {tick(true)}
            </DropdownMenuItem>
          )}
          <DropdownMenuSeparator />
          <DropdownMenuLabel className="text-xs text-muted-foreground">
            IP 管理（{proxies.length}）
          </DropdownMenuLabel>
          {filtered.map((p) => {
            const where = formatLocation(p.exit)
            return (
              <DropdownMenuItem
                key={p.id}
                onSelect={() => choose({ kind: 'pool', id: p.id })}
                className={cn('cursor-pointer gap-2', !p.enabled && 'opacity-60')}
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-1.5">
                    <span className="truncate font-medium">{p.name}</span>
                    <span
                      className="shrink-0 rounded bg-muted px-1.5 text-[11px] text-muted-foreground"
                      title="关联的账号数"
                    >
                      {p.credentialCount}
                    </span>
                    <ProxyHealthBadge proxy={p} />
                  </div>
                  <div className="truncate text-xs text-muted-foreground">
                    {p.url}
                    {where && ` · ${where}`}
                    {!p.enabled && ' · 已禁用'}
                  </div>
                </div>
                <button
                  type="button"
                  tabIndex={-1}
                  title="测试连通性"
                  className="shrink-0 rounded p-1 text-muted-foreground hover:bg-accent hover:text-emerald-600"
                  // 不触发菜单项的选中
                  onPointerDown={(e) => e.stopPropagation()}
                  onPointerUp={(e) => e.stopPropagation()}
                  onClick={(e) => {
                    e.preventDefault()
                    e.stopPropagation()
                    testOne.mutate(p.id)
                  }}
                  disabled={testingId === p.id}
                >
                  {testingId === p.id ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    <Play className="h-3.5 w-3.5" />
                  )}
                </button>
                {tick(isSelected({ kind: 'pool', id: p.id }))}
              </DropdownMenuItem>
            )
          })}
          {filtered.length === 0 && (
            <div className="px-2 py-3 text-center text-xs text-muted-foreground">
              {proxies.length === 0 ? '还没有保存的代理' : '没有匹配的代理'}
            </div>
          )}
          <DropdownMenuSeparator />
          <DropdownMenuItem
            className="cursor-pointer text-xs text-muted-foreground"
            onSelect={() => {
              window.location.hash = '#/proxies'
            }}
          >
            管理 IP…
          </DropdownMenuItem>
        </div>
      </DropdownMenuContent>
    </DropdownMenu>
  )
})
