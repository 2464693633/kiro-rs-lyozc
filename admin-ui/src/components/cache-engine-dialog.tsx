import { useEffect, useState } from 'react'
import { Database } from 'lucide-react'
import { toast } from 'sonner'

import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'
import {
  useCacheEnginesConfig, useCacheEnginesStats, useSetCacheEnginesConfig,
} from '@/hooks/use-cache-engines'
import type { CacheEngineCounters } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

/** 计数器一行。命中率按 hits/(hits+misses) 算，无样本时显示 — 而非 0%。 */
function CountersRow({ label, c }: { label: string; c?: CacheEngineCounters }) {
  const total = (c?.hits ?? 0) + (c?.misses ?? 0)
  const rate = total > 0 ? (((c?.hits ?? 0) / total) * 100).toFixed(1) + '%' : '—'
  return (
    <div className="flex items-center justify-between gap-2 text-[11px]">
      <span className="text-muted-foreground">{label}</span>
      <span className="font-mono">
        {c?.entries ?? 0}/{c?.capacity ?? 0} 条 · 命中 {rate} · 淘汰 {c?.evictions ?? 0} · 过期{' '}
        {c?.expirations ?? 0}
      </span>
    </div>
  )
}

const NUM = 'font-mono text-[13px]'

export function CacheEngineDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  // 仅在弹窗打开时拉取 / 轮询，避免常驻请求
  const { data: config } = useCacheEnginesConfig(open)
  const { data: stats } = useCacheEnginesStats(open)
  const setConfig = useSetCacheEnginesConfig()
  const saving = setConfig.isPending

  // 表单用字符串态：数字输入框清空时不该被强塞 0
  const [capacity, setCapacity] = useState('')
  const [maxTtl, setMaxTtl] = useState('')
  const [defaultTtl, setDefaultTtl] = useState('')
  const [maxRatio, setMaxRatio] = useState('')
  const [ttlSeconds, setTtlSeconds] = useState('')
  const [maxEntries, setMaxEntries] = useState('')
  const [minTokens, setMinTokens] = useState('')
  const [opusMinTokens, setOpusMinTokens] = useState('')
  const [goInputMul, setGoInputMul] = useState('')
  const [goCacheReadMul, setGoCacheReadMul] = useState('')
  const [goCacheCreationMul, setGoCacheCreationMul] = useState('')

  useEffect(() => {
    if (!config) return
    setCapacity(String(config.rust.capacity))
    setMaxTtl(String(config.rust.maxTtlSecs))
    setDefaultTtl(String(config.rust.defaultTtlSecs))
    setMaxRatio(String(config.go.maxRatio))
    setTtlSeconds(String(config.go.ttlSeconds))
    setMaxEntries(String(config.go.maxEntries))
    setMinTokens(String(config.go.minCacheableTokens))
    setOpusMinTokens(String(config.go.opusMinCacheableTokens))
    setGoInputMul(String(config.go.inputTokenMultiplier))
    setGoCacheReadMul(String(config.go.cacheReadMultiplier))
    setGoCacheCreationMul(String(config.go.cacheCreationMultiplier))
  }, [config])

  const handleSave = async (e: React.FormEvent) => {
    e.preventDefault()
    try {
      // 后端会再 sanitize 一次并回显生效值，故此处不做前端夹取
      const saved = await setConfig.mutateAsync({
        rust: {
          capacity: Number(capacity),
          maxTtlSecs: Number(maxTtl),
          defaultTtlSecs: Number(defaultTtl),
        },
        go: {
          maxRatio: Number(maxRatio),
          ttlSeconds: Number(ttlSeconds),
          maxEntries: Number(maxEntries),
          minCacheableTokens: Number(minTokens),
          opusMinCacheableTokens: Number(opusMinTokens),
          inputTokenMultiplier: Number(goInputMul),
          cacheReadMultiplier: Number(goCacheReadMul),
          cacheCreationMultiplier: Number(goCacheCreationMul),
        },
      })
      const clamped =
        saved.go.maxEntries !== Number(maxEntries) || saved.go.maxRatio !== Number(maxRatio)
      toast.success(clamped ? '已保存（部分非法值已夹到合法范围）' : '已保存，立即生效')
      onOpenChange(false)
    } catch (err) {
      toast.error('保存失败：' + extractErrorMessage(err))
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => { if (!saving) onOpenChange(o) }}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Database className="h-4 w-4" />
            缓存模拟引擎参数
          </DialogTitle>
          <DialogDescription>
            两套引擎各自独立配置，由客户端 Key 上的引擎选择决定走哪一套。保存后立即生效，无需重启。
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleSave} className="space-y-4 py-2">
          <div className="grid gap-4 sm:grid-cols-2">
            {/* 引擎 A */}
            <div className="space-y-3 rounded-md border p-3">
              <div>
                <div className="text-[13px] font-medium">rust（引擎 A）</div>
                <p className="text-[11px] text-muted-foreground">
                  按 session / Key 隔离前缀；主 Key 无 session 时不模拟缓存。
                </p>
                <p className="text-[11px] text-amber-600 dark:text-amber-500">
                  倍率不在这里 —— rust 引擎走顶栏「Token 膨胀倍率」那一套。
                </p>
              </div>
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground">条目上限</label>
                <Input type="number" min="1" value={capacity} disabled={saving}
                  onChange={(e) => setCapacity(e.target.value)} className={NUM} />
              </div>
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground">最长 TTL（秒）</label>
                <Input type="number" min="1" value={maxTtl} disabled={saving}
                  onChange={(e) => setMaxTtl(e.target.value)} className={NUM} />
              </div>
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground">默认 TTL（秒）</label>
                <Input type="number" min="1" value={defaultTtl} disabled={saving}
                  onChange={(e) => setDefaultTtl(e.target.value)} className={NUM} />
              </div>
            </div>

            {/* 引擎 B */}
            <div className="space-y-3 rounded-md border p-3">
              <div>
                <div className="text-[13px] font-medium">go（引擎 B）</div>
                <p className="text-[11px] text-muted-foreground">
                  移植自 kiro-go，全局共享指纹表 —— 不同 Key 的相同前缀会互相命中。
                </p>
              </div>
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground">
                  命中率上限（0.5–0.99，调大 → 更多 read）
                </label>
                <Input type="number" step="0.01" min="0.5" max="0.99" value={maxRatio} disabled={saving}
                  onChange={(e) => setMaxRatio(e.target.value)} className={NUM} />
              </div>
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground">
                  断点 TTL（秒，调小 → 更多 creation）
                </label>
                <Input type="number" min="1" value={ttlSeconds} disabled={saving}
                  onChange={(e) => setTtlSeconds(e.target.value)} className={NUM} />
              </div>
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground">条目上限（下限 256）</label>
                <Input type="number" min="256" value={maxEntries} disabled={saving}
                  onChange={(e) => setMaxEntries(e.target.value)} className={NUM} />
              </div>
              <div className="grid grid-cols-2 gap-2">
                <div className="space-y-1">
                  <label className="text-xs text-muted-foreground">最小可缓存 token</label>
                  <Input type="number" min="0" value={minTokens} disabled={saving}
                    onChange={(e) => setMinTokens(e.target.value)} className={NUM} />
                </div>
                <div className="space-y-1">
                  <label className="text-xs text-muted-foreground">Opus 最小值</label>
                  <Input type="number" min="0" value={opusMinTokens} disabled={saving}
                    onChange={(e) => setOpusMinTokens(e.target.value)} className={NUM} />
                </div>
              </div>
              <div className="grid grid-cols-2 gap-2">
                <div className="space-y-1">
                  <label className="text-xs text-muted-foreground">input 倍率</label>
                  <Input type="number" step="0.1" min="0.1" value={goInputMul} disabled={saving}
                    onChange={(e) => setGoInputMul(e.target.value)} className={NUM} />
                </div>
                <div className="space-y-1">
                  <label className="text-xs text-muted-foreground">cache_read 倍率</label>
                  <Input type="number" step="0.1" min="0.1" value={goCacheReadMul} disabled={saving}
                    onChange={(e) => setGoCacheReadMul(e.target.value)} className={NUM} />
                </div>
              </div>
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground">
                  cache_creation 倍率（1.0 = Go 原实现）
                </label>
                <Input type="number" step="0.1" min="0.1" value={goCacheCreationMul} disabled={saving}
                  onChange={(e) => setGoCacheCreationMul(e.target.value)} className={NUM} />
              </div>
              <p className="text-[11px] text-muted-foreground">
                go 引擎用这三个倍率，<strong>不走</strong>全局 Token 膨胀倍率。
                creation 保持 1.0 时与 Go 原实现一致；调离 1.0 会削弱两套引擎在
                creation/read 划分上的可比性。
              </p>
            </div>
          </div>

          <div className="space-y-1 rounded-md border bg-muted/30 p-3">
            <div className="text-[12px] font-medium">运行计数器</div>
            <CountersRow label="rust" c={stats?.rust} />
            <CountersRow label="go" c={stats?.go} />
          </div>

          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={saving}>
              取消
            </Button>
            <Button type="submit" disabled={saving}>
              {saving ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
