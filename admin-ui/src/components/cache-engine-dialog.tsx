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

const NUM = 'h-8 font-mono text-[12px]'

/** 四个倍率维度。顺序与后端四元组 `(input, output, cache_read, cache_creation)` 一致。 */
const MUL_KEYS = ['input', 'output', 'cacheRead', 'cacheCreation'] as const
type MulKey = (typeof MUL_KEYS)[number]

type EngineKey = 'rust' | 'go' | 'real' | 'nocache'

/** 每个引擎支持哪些倍率维度。D 的 cache 恒为 0，给旋钮只会误导。 */
const ENGINES: {
  key: EngineKey
  label: string
  hint: string
  /** 不支持的维度显示为 —，不渲染输入框 */
  omit?: MulKey[]
}[] = [
  { key: 'rust', label: 'A · rust', hint: '会话隔离前缀模拟' },
  { key: 'go', label: 'B · go', hint: '全局共享指纹表' },
  { key: 'real', label: 'C · real', hint: '上游真实 cache 划分' },
  { key: 'nocache', label: 'D · nocache', hint: '本地估算，cache 恒 0', omit: ['cacheRead', 'cacheCreation'] },
]

const MUL_LABELS: Record<MulKey, string> = {
  input: 'input',
  output: 'output',
  cacheRead: 'cache_read',
  cacheCreation: 'cache_create',
}

type MulState = Record<EngineKey, Record<MulKey, string>>

const emptyMuls = (): MulState => ({
  rust: { input: '', output: '', cacheRead: '', cacheCreation: '' },
  go: { input: '', output: '', cacheRead: '', cacheCreation: '' },
  real: { input: '', output: '', cacheRead: '', cacheCreation: '' },
  nocache: { input: '', output: '', cacheRead: '', cacheCreation: '' },
})

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

/** 带标签的紧凑数字输入。 */
function Field({
  label, value, onChange, disabled, min = '1', max, step = '1', hint,
}: {
  label: string
  value: string
  onChange: (v: string) => void
  disabled?: boolean
  min?: string
  max?: string
  step?: string
  hint?: string
}) {
  return (
    <label className="block space-y-1">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <Input
        type="number" min={min} max={max} step={step} value={value} disabled={disabled}
        onChange={(e) => onChange(e.target.value)} className={NUM} title={hint}
      />
    </label>
  )
}

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
  const [muls, setMuls] = useState<MulState>(emptyMuls)
  const [globalInput, setGlobalInput] = useState('')
  const [globalOutput, setGlobalOutput] = useState('')
  const [globalCache, setGlobalCache] = useState('')

  // 引擎 A 未显式配置时逐项回退这三个值，故输入框留空时用它们作 placeholder。
  const globalFallback: Record<MulKey, string> = {
    input: globalInput,
    output: globalOutput,
    cacheRead: globalCache,
    cacheCreation: globalCache,
  }

  const setMul = (engine: EngineKey, key: MulKey, value: string) =>
    setMuls((prev) => ({ ...prev, [engine]: { ...prev[engine], [key]: value } }))

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
    setGlobalInput(String(config.global?.inputMultiplier ?? 1))
    setGlobalOutput(String(config.global?.outputMultiplier ?? 1))
    setGlobalCache(String(config.global?.cacheMultiplier ?? 1))
    // A 的四个倍率是 Option：null = 继承全局，回显为空串而非填入全局值 ——
    // 否则一打开弹窗再保存就会把"继承"固化成显式值，静默切断后续全局调整。
    const opt = (v: number | null | undefined) => (v == null ? '' : String(v))
    setMuls({
      rust: {
        input: opt(config.rust.inputMultiplier),
        output: opt(config.rust.outputMultiplier),
        cacheRead: opt(config.rust.cacheReadMultiplier),
        cacheCreation: opt(config.rust.cacheCreationMultiplier),
      },
      go: {
        input: String(config.go.inputTokenMultiplier),
        output: String(config.go.outputMultiplier ?? 1),
        cacheRead: String(config.go.cacheReadMultiplier),
        cacheCreation: String(config.go.cacheCreationMultiplier),
      },
      real: {
        input: String(config.real?.inputMultiplier ?? 1),
        output: String(config.real?.outputMultiplier ?? 1),
        cacheRead: String(config.real?.cacheReadMultiplier ?? 1),
        cacheCreation: String(config.real?.cacheCreationMultiplier ?? 1),
      },
      nocache: {
        input: String(config.nocache?.inputMultiplier ?? 1),
        output: String(config.nocache?.outputMultiplier ?? 1),
        cacheRead: '',
        cacheCreation: '',
      },
    })
  }, [config])

  const handleSave = async (e: React.FormEvent) => {
    e.preventDefault()
    // A 用 optNum：空串 → null（继承全局）。B/C/D 用 num：空串 → 1（不缩放），
    // 它们没有"继承"语义，留空只能理解为不缩放。
    const optNum = (v: string): number | null => {
      const t = v.trim()
      if (t === '') return null
      const n = Number(t)
      return Number.isFinite(n) ? n : null
    }
    const num = (v: string) => {
      const n = Number(v)
      return Number.isFinite(n) && n > 0 ? n : 1
    }
    try {
      // 后端会再 sanitize 一次并回显生效值，故此处不做前端夹取
      const saved = await setConfig.mutateAsync({
        rust: {
          capacity: Number(capacity),
          maxTtlSecs: Number(maxTtl),
          defaultTtlSecs: Number(defaultTtl),
          inputMultiplier: optNum(muls.rust.input),
          outputMultiplier: optNum(muls.rust.output),
          cacheReadMultiplier: optNum(muls.rust.cacheRead),
          cacheCreationMultiplier: optNum(muls.rust.cacheCreation),
        },
        go: {
          maxRatio: Number(maxRatio),
          ttlSeconds: Number(ttlSeconds),
          maxEntries: Number(maxEntries),
          minCacheableTokens: Number(minTokens),
          opusMinCacheableTokens: Number(opusMinTokens),
          inputTokenMultiplier: num(muls.go.input),
          outputMultiplier: num(muls.go.output),
          cacheReadMultiplier: num(muls.go.cacheRead),
          cacheCreationMultiplier: num(muls.go.cacheCreation),
        },
        real: {
          inputMultiplier: num(muls.real.input),
          outputMultiplier: num(muls.real.output),
          cacheReadMultiplier: num(muls.real.cacheRead),
          cacheCreationMultiplier: num(muls.real.cacheCreation),
        },
        nocache: {
          inputMultiplier: num(muls.nocache.input),
          outputMultiplier: num(muls.nocache.output),
        },
        global: {
          inputMultiplier: num(globalInput),
          outputMultiplier: num(globalOutput),
          cacheMultiplier: num(globalCache),
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
      <DialogContent className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Database className="h-4 w-4" />
            缓存模拟引擎
          </DialogTitle>
          <DialogDescription>
            四套引擎各自独立配置，由客户端 Key 上的引擎选择决定走哪一套。保存后立即生效，无需重启。
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleSave} className="space-y-4 py-1">
          {/* 倍率矩阵：四引擎 × 四倍率，横向对齐便于直接比较同一维度上的差异 */}
          <div className="space-y-2">
            <div className="flex items-baseline justify-between">
              <div className="text-[13px] font-medium">下发倍率</div>
              <div className="text-[11px] text-muted-foreground">
                作用在下发给客户端的 usage 上（计费按此记录）
              </div>
            </div>
            <div className="overflow-x-auto rounded-md border">
              <table className="w-full min-w-[560px] border-collapse text-[12px]">
                <thead>
                  <tr className="border-b bg-muted/40">
                    <th className="w-[168px] px-2 py-1.5 text-left font-medium">引擎</th>
                    {MUL_KEYS.map((k) => (
                      <th key={k} className="px-2 py-1.5 text-left font-medium">
                        {MUL_LABELS[k]}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {ENGINES.map((eng) => (
                    <tr key={eng.key} className="border-b last:border-0">
                      <td className="px-2 py-1.5 align-middle">
                        <div className="font-mono text-[12px]">{eng.key}</div>
                        <div className="text-[10px] leading-tight text-muted-foreground">
                          {eng.label}
                        </div>
                      </td>
                      {MUL_KEYS.map((k) => {
                        // D 的 cache 恒为 0：不给旋钮，避免运维误以为调它有用
                        if (eng.key === 'nocache' && (k === 'cacheRead' || k === 'cacheCreation')) {
                          return (
                            <td key={k} className="px-2 py-1.5 text-center text-muted-foreground">
                              <span title="引擎 D 的 cache 恒为 0，无对应倍率">—</span>
                            </td>
                          )
                        }
                        return (
                          <td key={k} className="px-2 py-1.5">
                            <Input
                              type="number"
                              step="0.1"
                              min="0"
                              value={muls[eng.key][k]}
                              disabled={saving}
                              // A 留空 = 继承全局，故用全局值作 placeholder
                              placeholder={eng.key === 'rust' ? globalFallback[k] : undefined}
                              onChange={(e) => setMul(eng.key, k, e.target.value)}
                              className={NUM}
                            />
                          </td>
                        )
                      })}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <p className="text-[11px] text-muted-foreground">
              引擎 A 留空 = 继承下方全局倍率（灰字为当前继承值）。B / C / D 无继承语义，留空按 1.0 处理。
            </p>
          </div>

          {/* 全局倍率：A 的回退来源，同时也是顶栏那个入口的同一份存储 */}
          <div className="space-y-2 rounded-md border p-3">
            <div>
              <div className="text-[13px] font-medium">全局膨胀倍率</div>
              <p className="text-[11px] text-muted-foreground">
                引擎 A 未显式设置某项时回退到这里。与顶栏「Token 膨胀倍率」是同一份存储。
              </p>
            </div>
            <div className="grid gap-2 sm:grid-cols-3">
              <Field label="input" value={globalInput} onChange={setGlobalInput}
                disabled={saving} step="0.01" min="1" />
              <Field label="output" value={globalOutput} onChange={setGlobalOutput}
                disabled={saving} step="0.01" min="1" />
              <Field label="cache（read / create 共用）" value={globalCache} onChange={setGlobalCache}
                disabled={saving} step="0.01" min="1" />
            </div>
          </div>

          {/* 缓存参数：只有 A / B 有缓存状态，C / D 无指纹表故无参数可调 */}
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="space-y-2 rounded-md border p-3">
              <div>
                <div className="font-mono text-[12px]">rust</div>
                <p className="text-[11px] text-muted-foreground">
                  按 session / Key 隔离前缀；主 Key 无 session 时不模拟缓存。
                </p>
              </div>
              <Field label="条目上限" value={capacity} onChange={setCapacity}
                disabled={saving} min="1" />
              <Field label="最长 TTL（秒）" value={maxTtl} onChange={setMaxTtl}
                disabled={saving} min="1" />
              <Field label="默认 TTL（秒）" value={defaultTtl} onChange={setDefaultTtl}
                disabled={saving} min="1" />
            </div>

            <div className="space-y-2 rounded-md border p-3">
              <div>
                <div className="font-mono text-[12px]">go</div>
                <p className="text-[11px] text-muted-foreground">
                  移植自 kiro-go，按账号共享指纹表 —— 同账号下相同前缀会互相命中。
                </p>
              </div>
              <Field label="命中率上限（0.5–0.99，调大 → 更多 read）" value={maxRatio}
                onChange={setMaxRatio} disabled={saving} step="0.01" min="0.5" max="0.99" />
              <Field label="断点 TTL（秒，调小 → 更多 creation）" value={ttlSeconds}
                onChange={setTtlSeconds} disabled={saving} min="1" />
              <Field label="条目上限（下限 256）" value={maxEntries}
                onChange={setMaxEntries} disabled={saving} min="256" />
              <div className="grid grid-cols-2 gap-2">
                <Field label="最小可缓存 token" value={minTokens}
                  onChange={setMinTokens} disabled={saving} min="0" />
                <Field label="Opus 最小值" value={opusMinTokens}
                  onChange={setOpusMinTokens} disabled={saving} min="0" />
              </div>
            </div>
          </div>

          {/* 计数器：C / D 无缓存状态，故不列 —— 填零会让人以为"有缓存但没命中" */}
          <div className="space-y-1 rounded-md border bg-muted/30 p-3">
            <div className="text-[12px] font-medium">运行计数器</div>
            <CountersRow label="rust" c={stats?.rust} />
            <CountersRow label="go" c={stats?.go} />
            <p className="pt-0.5 text-[10px] text-muted-foreground">
              real / nocache 无缓存状态（无指纹表、无 TTL），故无命中率可言。
            </p>
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
