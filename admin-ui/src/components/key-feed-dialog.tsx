import { useEffect, useState } from 'react'
import { Rss, Loader2, CheckCircle2, XCircle, AlertCircle } from 'lucide-react'
import { toast } from 'sonner'

import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'
import { GroupMultiSelect } from '@/components/group-select'
import { useGroupOptions } from '@/hooks/use-groups'
import { useKeyFeedConfig, usePullKeyFeed, useSetKeyFeedConfig } from '@/hooks/use-key-feed'
import type { KeyFeedItemResult, KeyFeedPullResult } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

/** 与后端 KEY_FEED_MIN_INTERVAL_SECS 对齐；低于此值后端会夹取 */
const MIN_INTERVAL_SECS = 60

const FIELD = 'h-8 font-mono text-[12px]'

/** RFC3339 → 本地时间短格式。非法值原样返回，不编造时间。 */
function formatTime(value?: string): string {
  if (!value) return '—'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return value
  return date.toLocaleString(undefined, {
    month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit',
  })
}

/** 距今剩余时长的粗粒度描述（用于保修到期）。已过期显式标注。 */
function formatRemaining(value?: string): string | null {
  if (!value) return null
  const target = new Date(value).getTime()
  if (Number.isNaN(target)) return null
  const diffMs = target - Date.now()
  if (diffMs <= 0) return '已过保'
  const minutes = Math.round(diffMs / 60_000)
  if (minutes < 60) return `剩 ${minutes} 分钟`
  const hours = Math.floor(minutes / 60)
  const rest = minutes % 60
  return rest > 0 ? `剩 ${hours} 小时 ${rest} 分` : `剩 ${hours} 小时`
}

const STATUS_META: Record<
  KeyFeedItemResult['status'],
  { label: string; className: string; Icon: typeof CheckCircle2 }
> = {
  verified: { label: '已验活', className: 'text-emerald-600', Icon: CheckCircle2 },
  imported: { label: '已导入', className: 'text-emerald-600', Icon: CheckCircle2 },
  duplicate: { label: '已存在', className: 'text-muted-foreground', Icon: AlertCircle },
  failed: { label: '失败', className: 'text-destructive', Icon: XCircle },
}

/** 最近一次拉取的汇总 + 明细。 */
function LastPullPanel({ result }: { result: KeyFeedPullResult }) {
  const remaining = formatRemaining(result.earliestWarrantyUntil)

  return (
    <div className="space-y-2 rounded-md border bg-muted/30 p-2.5">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[12px] font-medium">最近一次拉取</span>
        <span className="font-mono text-[11px] text-muted-foreground">
          {formatTime(result.pulledAt)}
        </span>
      </div>

      {result.success ? (
        <div className="font-mono text-[11px] leading-relaxed">
          号位 {result.total} · 候选 {result.candidates} · 新增{' '}
          <span className={result.imported > 0 ? 'text-emerald-600' : ''}>{result.imported}</span>
          {' '}· 已存在 {result.duplicate} · 失败{' '}
          <span className={result.failed > 0 ? 'text-destructive' : ''}>{result.failed}</span>
          {result.reaped > 0 && <> · 清理 {result.reaped}</>}
        </div>
      ) : (
        <div className="text-[11px] text-destructive">
          拉取失败：{result.error ?? '未知错误'}
        </div>
      )}

      {result.earliestWarrantyUntil && (
        <div className="text-[11px] text-muted-foreground">
          最早到保：{formatTime(result.earliestWarrantyUntil)}
          {remaining && <span className="ml-1">（{remaining}）</span>}
        </div>
      )}

      {/* 订阅源自己的提示，比如「暂无可用号，正在补货」 */}
      {result.message && (
        <div className="rounded bg-background/60 px-2 py-1 text-[11px] text-muted-foreground">
          源提示：{result.message}
        </div>
      )}

      {result.items && result.items.length > 0 && (
        <div className="max-h-40 space-y-1 overflow-y-auto pt-0.5">
          {result.items.map((item, index) => {
            const meta = STATUS_META[item.status] ?? STATUS_META.failed
            const { Icon } = meta
            return (
              <div key={`${item.label}-${index}`} className="flex items-start gap-1.5 text-[11px]">
                <Icon className={`mt-0.5 h-3 w-3 shrink-0 ${meta.className}`} />
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-baseline gap-x-1.5">
                    <span className="font-mono">{item.label}</span>
                    {item.apiRegion && (
                      <span className="text-muted-foreground">{item.apiRegion}</span>
                    )}
                    {item.credentialId != null && (
                      <span className="text-muted-foreground">#{item.credentialId}</span>
                    )}
                    <span className={meta.className}>{meta.label}</span>
                  </div>
                  {item.error && (
                    <div className="break-words text-destructive">{item.error}</div>
                  )}
                </div>
              </div>
            )
          })}
        </div>
      )}

      {/* 明细为空但有候选：说明全是去重（后端不记录去重明细，避免刷屏） */}
      {result.success && result.candidates > 0 && (!result.items || result.items.length === 0) && (
        <p className="text-[11px] text-muted-foreground">
          本轮全部号位已在库中（去重明细不单独列出）。
        </p>
      )}
    </div>
  )
}

export function KeyFeedDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { data: config } = useKeyFeedConfig(open)
  const setConfig = useSetKeyFeedConfig()
  const pull = usePullKeyFeed()
  const groupOptions = useGroupOptions()

  const saving = setConfig.isPending
  const pulling = pull.isPending
  const busy = saving || pulling

  const [enabled, setEnabled] = useState(false)
  const [url, setUrl] = useState('')
  const [token, setToken] = useState('')
  const [tokenParam, setTokenParam] = useState('token')
  const [intervalSecs, setIntervalSecs] = useState('')
  const [verify, setVerify] = useState(true)
  const [priority, setPriority] = useState('')
  const [groups, setGroups] = useState<string[]>([])
  const [endpoint, setEndpoint] = useState('')
  const [reapDisabled, setReapDisabled] = useState(false)

  // 拉取返回的结果优先于配置里的 lastPull：手动拉完立刻看到本次结果，
  // 不用等 invalidate 回来。
  const lastPull = pull.data ?? config?.lastPull

  useEffect(() => {
    if (!config) return
    setEnabled(config.enabled)
    setUrl(config.url ?? '')
    // token 后端从不回显明文，留空表示"保持原值"
    setToken('')
    setTokenParam(config.tokenParam || 'token')
    setIntervalSecs(String(config.intervalSecs))
    setVerify(config.verify)
    setPriority(String(config.priority))
    setGroups(config.groups ?? [])
    setEndpoint(config.endpoint ?? '')
    setReapDisabled(config.reapDisabled)
  }, [config])

  const buildPatch = () => {
    const interval = Number(intervalSecs)
    const prio = Number(priority)
    return {
      enabled,
      url: url.trim(),
      // 空串会被后端理解为"清除 token"，所以未输入时干脆不传该字段
      ...(token.trim() !== '' ? { token: token.trim() } : {}),
      // 与 url 同批提交，后端用它去拆链接里的令牌
      tokenParam: tokenParam.trim() || 'token',
      intervalSecs: Number.isFinite(interval) && interval > 0 ? interval : MIN_INTERVAL_SECS,
      verify,
      priority: Number.isFinite(prio) && prio >= 0 ? prio : 0,
      groups,
      endpoint: endpoint.trim(),
      reapDisabled,
    }
  }

  const handleSave = async (e: React.FormEvent) => {
    e.preventDefault()
    try {
      const saved = await setConfig.mutateAsync(buildPatch())
      setToken('')
      const clamped = saved.intervalSecs !== Number(intervalSecs)
      toast.success(
        clamped
          ? `已保存（轮询间隔已夹到 ${saved.intervalSecs} 秒）`
          : '已保存，立即生效',
      )
    } catch (err) {
      toast.error('保存失败：' + extractErrorMessage(err))
    }
  }

  /** 先保存再拉取：避免用户改了 URL 直接点「立即拉取」时打的还是旧地址。 */
  const handlePull = async () => {
    try {
      await setConfig.mutateAsync(buildPatch())
      setToken('')
    } catch (err) {
      toast.error('保存配置失败，未执行拉取：' + extractErrorMessage(err))
      return
    }

    try {
      const result = await pull.mutateAsync()
      if (!result.success) {
        toast.error('拉取失败：' + (result.error ?? '未知错误'))
        return
      }
      if (result.imported > 0) {
        toast.success(`拉取完成，新增 ${result.imported} 个凭据`)
      } else if (result.candidates === 0) {
        toast.info(result.message ? `订阅源：${result.message}` : '订阅源当前无可用号位')
      } else {
        toast.info(`拉取完成，${result.candidates} 个号位均已在库中`)
      }
    } catch (err) {
      toast.error('拉取失败：' + extractErrorMessage(err))
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => { if (!busy) onOpenChange(o) }}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Rss className="h-4 w-4" />
            API Key 订阅源
          </DialogTitle>
          <DialogDescription>
            定期拉取拼车链接，把其中的新 API Key 自动导入为凭据。已入库的 key
            按 kiroApiKey 指纹去重，重复拉取不产生上游开销。
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleSave} className="space-y-3 py-1">
          {/* 总开关 */}
          <div className="flex items-center justify-between rounded-md border px-2.5 py-2">
            <div className="space-y-0.5">
              <div className="text-[13px] font-medium">后台自动轮询</div>
              <div className="text-[11px] text-muted-foreground">
                关闭后仅「立即拉取」可用，调度器空转
              </div>
            </div>
            <Switch checked={enabled} disabled={busy} onCheckedChange={setEnabled} />
          </div>

          {/* 订阅源地址 */}
          <div className="space-y-1">
            <label className="text-[11px] text-muted-foreground">
              订阅源链接（任意地址，直接粘贴完整 URL）
            </label>
            <Input
              value={url}
              disabled={busy}
              placeholder="https://your-source.example.com/get?token=..."
              onChange={(e) => setUrl(e.target.value)}
              className={FIELD}
            />
            <p className="text-[11px] leading-relaxed text-muted-foreground">
              换源只改这里即可，返回格式相同就能直接用。保存时会按右侧的参数名把令牌拆到
              独立字段，接口不再回显明文。
            </p>
          </div>

          {/* 访问令牌 + 参数名 */}
          <div className="grid grid-cols-[1fr_9rem] gap-2">
            <div className="space-y-1">
              <label className="text-[11px] text-muted-foreground">
                访问令牌
                {config?.tokenSet && <span className="ml-1 text-emerald-600">（已配置）</span>}
              </label>
              <Input
                type="password"
                value={token}
                disabled={busy}
                placeholder={config?.tokenSet ? '留空保持不变' : '未配置'}
                onChange={(e) => setToken(e.target.value)}
                className={FIELD}
                autoComplete="off"
              />
            </div>
            <div className="space-y-1">
              <label className="text-[11px] text-muted-foreground">令牌参数名</label>
              <Input
                value={tokenParam}
                disabled={busy}
                placeholder="token"
                onChange={(e) => setTokenParam(e.target.value)}
                className={FIELD}
              />
            </div>
          </div>
          <p className="-mt-1 text-[11px] leading-relaxed text-muted-foreground">
            参数名按源而定（<code>token</code> / <code>key</code> / <code>accessKey</code>…），
            大小写不敏感。填对了才能把令牌从链接里摘出来单独存放。
          </p>

          {/* 轮询间隔 / 优先级 */}
          <div className="grid grid-cols-2 gap-2">
            <div className="space-y-1">
              <label className="text-[11px] text-muted-foreground">
                轮询间隔（秒，最小 {MIN_INTERVAL_SECS}）
              </label>
              <Input
                type="number"
                min={String(MIN_INTERVAL_SECS)}
                step="30"
                value={intervalSecs}
                disabled={busy}
                onChange={(e) => setIntervalSecs(e.target.value)}
                className={FIELD}
              />
            </div>
            <div className="space-y-1">
              <label className="text-[11px] text-muted-foreground">导入优先级（越小越先）</label>
              <Input
                type="number"
                min="0"
                step="1"
                value={priority}
                disabled={busy}
                onChange={(e) => setPriority(e.target.value)}
                className={FIELD}
              />
            </div>
          </div>

          {/* 端点 */}
          <div className="space-y-1">
            <label className="text-[11px] text-muted-foreground">端点名（留空用全局默认）</label>
            <Input
              value={endpoint}
              disabled={busy}
              placeholder="ide"
              onChange={(e) => setEndpoint(e.target.value)}
              className={FIELD}
            />
          </div>

          {/* 分组 */}
          <div className="space-y-1">
            <label className="text-[11px] text-muted-foreground">导入到分组</label>
            <GroupMultiSelect
              value={groups}
              options={groupOptions}
              onChange={setGroups}
              disabled={busy}
            />
          </div>

          {/* 验活 */}
          <div className="flex items-center justify-between rounded-md border px-2.5 py-2">
            <div className="space-y-0.5">
              <div className="text-[13px] font-medium">导入时验活</div>
              <div className="text-[11px] text-muted-foreground">
                建议开启：顺带填上订阅等级，否则未知等级的 Free 号会被放进 Opus 调度
              </div>
            </div>
            <Switch checked={verify} disabled={busy} onCheckedChange={setVerify} />
          </div>

          {/* 清理 */}
          <div className="flex items-center justify-between rounded-md border px-2.5 py-2">
            <div className="space-y-0.5">
              <div className="text-[13px] font-medium">清理已自动禁用的号</div>
              <div className="text-[11px] text-muted-foreground">
                每轮结束后删除本源导入、且被系统自动禁用的凭据。手动禁用的保留
              </div>
            </div>
            <Switch checked={reapDisabled} disabled={busy} onCheckedChange={setReapDisabled} />
          </div>

          {lastPull && <LastPullPanel result={lastPull} />}

          <DialogFooter className="gap-2 sm:justify-between">
            <Button type="button" variant="secondary" onClick={handlePull} disabled={busy}>
              {pulling ? (
                <>
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  拉取中…
                </>
              ) : (
                '立即拉取'
              )}
            </Button>
            <div className="flex gap-2">
              <Button
                type="button"
                variant="outline"
                onClick={() => onOpenChange(false)}
                disabled={busy}
              >
                关闭
              </Button>
              <Button type="submit" disabled={busy}>
                {saving ? '保存中…' : '保存'}
              </Button>
            </div>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
