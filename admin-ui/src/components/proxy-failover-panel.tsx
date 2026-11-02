import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Loader2, Shuffle } from 'lucide-react'
import { toast } from 'sonner'
import { getProxyFailoverConfig, rotateCredentialProxies, setProxyFailoverConfig } from '@/api/credentials'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Switch } from '@/components/ui/switch'
import { extractErrorMessage } from '@/lib/utils'

export function ProxyFailoverPanel() {
  const queryClient = useQueryClient()
  const query = useQuery({ queryKey: ['proxy-failover'], queryFn: getProxyFailoverConfig })
  const update = useMutation({
    mutationFn: setProxyFailoverConfig,
    onSuccess: (config) => {
      queryClient.setQueryData(['proxy-failover'], config)
      toast.success('代理故障转移设置已保存')
    },
    onError: (error) => toast.error(`保存失败：${extractErrorMessage(error)}`),
  })
  const rotate = useMutation({
    mutationFn: rotateCredentialProxies,
    onSuccess: (result) => {
      toast.success(`轮换 ${result.rotated} 个账号，恢复代理 ${result.restoredToProxy} 个，临时直连 ${result.switchedToDirect} 个，保持原配置 ${result.unresolved} 个`)
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => toast.error(`轮换失败：${extractErrorMessage(error)}`),
  })
  const busy = update.isPending || rotate.isPending

  return (
    <Card className="space-y-3 p-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h2 className="text-sm font-medium">代理故障转移</h2>
          <p className="mt-1 text-xs text-muted-foreground">巡检会自动发现正在使用的代理；自动禁用的代理恢复连通后重新启用，手动禁用的保持禁用。</p>
        </div>
        <Button size="sm" variant="outline" onClick={() => rotate.mutate()} disabled={busy || !query.data?.autoRotateEnabled}>
          {rotate.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Shuffle className="h-3.5 w-3.5" />}
          立即轮换
        </Button>
      </div>
      {query.data ? (
        <div className="grid gap-3 md:grid-cols-2">
          <div className="flex items-start justify-between gap-3">
            <div>
              <label htmlFor="proxy-auto-rotate" className="text-xs font-medium">故障时自动轮换代理</label>
              <p className="mt-1 text-xs text-muted-foreground">账号当前代理不可用时，切换到池内其它可用代理。</p>
            </div>
            <Switch id="proxy-auto-rotate" checked={query.data.autoRotateEnabled} disabled={busy}
              onCheckedChange={(value) => update.mutate({ autoRotateEnabled: value })} />
          </div>
          <div className="flex items-start justify-between gap-3">
            <div>
              <label htmlFor="proxy-direct-fallback" className="text-xs font-medium">无可用代理时允许直连</label>
              <p className="mt-1 text-xs text-muted-foreground">开启后可临时使用服务器出口，代理恢复后自动切回；关闭时保持原代理。</p>
            </div>
            <Switch id="proxy-direct-fallback" checked={query.data.directFallbackEnabled} disabled={busy}
              onCheckedChange={(value) => update.mutate({ directFallbackEnabled: value })} />
          </div>
        </div>
      ) : query.isError ? (
        <div className="flex items-center gap-2 text-xs text-destructive">
          设置加载失败：{extractErrorMessage(query.error)}
          <Button size="sm" variant="ghost" onClick={() => query.refetch()} disabled={query.isFetching}>重试</Button>
        </div>
      ) : <p className="text-xs text-muted-foreground">正在加载设置…</p>}
    </Card>
  )
}
