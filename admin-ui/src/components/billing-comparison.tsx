import { useEffect, useMemo, useState } from 'react'
import { Save, Plus, Trash2 } from 'lucide-react'
import { Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis, Legend } from 'recharts'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { useCredentials } from '@/hooks/use-credentials'
import { useBillingComparison, useBillingConfig, useSetBillingConfig } from '@/hooks/use-billing'
import type { ModelPricing, StatsFilter, StatsTimeFilter } from '@/types/api'
import { formatNumber } from '@/lib/utils'

interface Props { timeFilter: StatsTimeFilter; statsFilter?: StatsFilter }

const emptyPrice = (): ModelPricing => ({
  inputPerMillion: 0,
  outputPerMillion: 0,
  cacheCreationPerMillion: 0,
  cacheReadPerMillion: 0,
})

function money(value: number) { return `$${value.toFixed(4)}` }

const ENGINE_LABELS: Record<string, string> = {
  rust: 'Rust 缓存',
  go: 'Go 缓存',
  real: '真实上游',
  nocache: '无缓存',
}

const ENGINE_COLORS: Record<string, string> = {
  rust: '#8884d8',
  go: '#82ca9d',
  real: '#ffc658',
  nocache: '#ff7c7c',
}

export function BillingComparison({ timeFilter, statsFilter }: Props) {
  const { data: comparison } = useBillingComparison(timeFilter, statsFilter)
  const { data: config } = useBillingConfig()
  const setConfig = useSetBillingConfig()
  const { data: credentials } = useCredentials()

  const [localConfig, setLocalConfig] = useState(config)
  useEffect(() => { if (config) setLocalConfig(config) }, [config])

  const upstreamCredentials = useMemo(
    () => credentials?.credentials.filter((c) => c.provider === 'anthropic' || c.provider === 'upstream') ?? [],
    [credentials]
  )

  const handleSave = () => { if (localConfig) setConfig.mutate(localConfig) }

  const updateModelPrice = (model: string, field: keyof ModelPricing, value: string) => {
    if (!localConfig) return
    const parsed = parseFloat(value)
    if (isNaN(parsed)) return
    setLocalConfig({
      ...localConfig,
      modelPrices: {
        ...localConfig.modelPrices,
        [model]: { ...

(localConfig.modelPrices[model] || emptyPrice()), [field]: parsed },
      },
    })
  }

  const addModel = () => {
    if (!localConfig) return
    const name = prompt('模型名称（如 claude-opus-4-20250514）：')
    if (!name || localConfig.modelPrices[name]) return
    setLocalConfig({ ...localConfig, modelPrices: { ...localConfig.modelPrices, [name]: emptyPrice() } })
  }

  const removeModel = (model: string) => {
    if (!localConfig) return
    const { [model]: _, ...rest } = localConfig.modelPrices
    setLocalConfig({ ...localConfig, modelPrices: rest })
  }

  const updateUpstreamMultiplier = (credId: number, value: string) => {
    if (!localConfig) return
    const parsed = parseFloat(value)
    if (isNaN(parsed)) return
    setLocalConfig({ ...localConfig, upstreamMultipliers: { ...localConfig.upstreamMultipliers, [credId]: parsed } })
  }

  const updateEngineMultiplier = (engine: 'rust' | 'go' | 'real' | 'nocache', value: string) => {
    if (!localConfig) return
    const parsed = parseFloat(value)
    if (isNaN(parsed)) return
    setLocalConfig({ ...localConfig, [`${engine}Multiplier`]: parsed })
  }

  // 图表数据：每个引擎各一条线
  const chartData = useMemo(() => {
    if (!comparison?.points) return []
    return comparison.points.map((point) => {
      const row: Record<string, any> = { ts: new Date(point.ts).toLocaleString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' }) }
      point.engines.forEach((e) => {
        row[`${e.engine}_upstream`] = e.upstreamCost
        row[`${e.engine}_client`] = e.clientCost
      })
      return row
    })
  }, [comparison])

  const engines = useMemo(() => comparison?.engines ?? [], [comparison])

  // 按引擎排序：rust / go / real / nocache，其余按字母序
  const sortedEngines = useMemo(() => {
    const order = ['rust', 'go', 'real', 'nocache']
    return [...engines].sort((a, b) => {
      const aIdx = order.indexOf(a.engine)
      const bIdx = order.indexOf(b.engine)
      if (aIdx !== -1 && bIdx !== -1) return aIdx - bIdx
      if (aIdx !== -1) return -1
      if (bIdx !== -1) return 1
      return a.engine.localeCompare(b.engine)
    })
  }, [engines])

  return <div className="space-y-4">
    <Card><CardHeader><CardTitle>逐引擎计费对比</CardTitle></CardHeader><CardContent>
      <div className="mb-4 text-sm text-muted-foreground">
        每个引擎行的「上游真实」与「客户端计费」来自<strong>同一批请求</strong>，故可直接相除得加价倍数。总计 {comparison?.calls ?? 0} 次调用。
      </div>

      {sortedEngines.length === 0 ? (
        <div className="py-8 text-center text-sm text-muted-foreground">当前时段无上游凭据流量</div>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>引擎</TableHead>
              <TableHead className="text-right">上游真实</TableHead>
              <TableHead className="text-right">客户端计费</TableHead>
              <TableHead className="text-right">差额</TableHead>
              <TableHead className="text-right">倍数</TableHead>
              <TableHead className="text-right">调用数</TableHead>
              <TableHead className="text-right">Tokens (上游)</TableHead>
              <TableHead className="text-right">Tokens (客户端)</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {sortedEngines.map((e) => {
              const diff = e.clientCost - e.upstreamCost
              const ratio = e.upstreamCost > 0 ? e.clientCost / e.upstreamCost : 0
              return (
                <TableRow key={e.engine}>
                  <TableCell className="font-medium">{ENGINE_LABELS[e.engine] || e.engine}</TableCell>
                  <TableCell className="text-right tabular-nums">{money(e.upstreamCost)}</TableCell>
                  <TableCell className="text-right tabular-nums">{money(e.clientCost)}</TableCell>
                  <TableCell className={`text-right tabular-nums ${diff >= 0 ? 'text-green-600' : 'text-red-600'}`}>
                    {diff >= 0 ? '+' : ''}{money(diff)}
                  </TableCell>
                  <TableCell className="text-right tabular-nums">{ratio.toFixed(2)}×</TableCell>
                  <TableCell className="text-right tabular-nums">{formatNumber(e.calls)}</TableCell>
                  <TableCell className="text-right tabular-nums text-xs text-muted-foreground">{formatNumber(e.upstreamTokens)}</TableCell>
                  <TableCell className="text-right tabular-nums text-xs text-muted-foreground">{formatNumber(e.clientTokens)}</TableCell>
                </TableRow>
              )
            })}
            <TableRow className="border-t-2 font-semibold">
              <TableCell>总计</TableCell>
              <TableCell className="text-right tabular-nums">{money(comparison?.upstreamCost ?? 0)}</TableCell>
              <TableCell className="text-right tabular-nums">{money(comparison?.clientCost ?? 0)}</TableCell>
              <TableCell className={`text-right tabular-nums ${(comparison?.clientCost ?? 0) >= (comparison?.upstreamCost ?? 0) ? 'text-green-600' : 'text-red-600'}`}>
                {((comparison?.clientCost ?? 0) - (comparison?.upstreamCost ?? 0)) >= 0 ? '+' : ''}{money((comparison?.clientCost ?? 0) - (comparison?.upstreamCost ?? 0))}
              </TableCell>
              <TableCell className="text-right tabular-nums">
                {(comparison?.upstreamCost ?? 0) > 0 ? ((comparison?.clientCost ?? 0) / (comparison?.upstreamCost ?? 0)).toFixed(2) : '—'}×
              </TableCell>
              <TableCell className="text-right tabular-nums">{formatNumber(comparison?.calls ?? 0)}</TableCell>
              <TableCell className="text-right tabular-nums text-xs text-muted-foreground">
                {formatNumber(sortedEngines.reduce((sum, e) => sum + e.upstreamTokens, 0))}
              </TableCell>
              <TableCell className="text-right tabular-nums text-xs text-muted-foreground">
                {formatNumber(sortedEngines.reduce((sum, e) => sum + e.clientTokens, 0))}
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      )}

      <div className="mt-6 h-64">
        <ResponsiveContainer width="100%" height="100%">
          <LineChart data={chartData}>
            <XAxis dataKey="ts" tick={{ fontSize: 11 }} />
            <YAxis tick={{ fontSize: 11 }} tickFormatter={(v) => `$${v.toFixed(2)}`} />
            <Tooltip formatter={(value: number) => money(value)} labelStyle={{ color: '#000' }} />
            <Legend wrapperStyle={{ fontSize: 12 }} />
            {sortedEngines.map((e) => (
              <Line
                key={`${e.engine}_client`}
                type="monotone"
                dataKey={`${e.engine}_client`}
                name={`${ENGINE_LABELS[e.engine] || e.engine} (客户端)`}
                stroke={ENGINE_COLORS[e.engine] || '#888'}
                strokeWidth={2}
                dot={false}
              />
            ))}
          </LineChart>
        </ResponsiveContainer>
      </div>
    </CardContent></Card>

    <Card><CardHeader className="flex flex-row items-center justify-between"><CardTitle>计费配置</CardTitle><Button onClick={handleSave} disabled={!localConfig || setConfig.isPending}><Save className="mr-1.5 size-4" />保存</Button></CardHeader><CardContent className="space-y-6">
      <div><h3 className="mb-3 text-sm font-semibold">模型价格（美元 / 百万 tokens）</h3><div className="space-y-3">{localConfig && Object.entries(localConfig.modelPrices).map(([model, price]) => <div key={model} className="grid grid-cols-[1fr_auto] gap-3 rounded-md border p-3"><div className="grid gap-2 sm:grid-cols-4"><div className="text-sm font-medium sm:col-span-4">{model}</div><Multiplier label="Input" value={price.inputPerMillion} onChange={(v) => updateModelPrice(model, 'inputPerMillion', v)} /><Multiplier label="Output" value={price.outputPerMillion} onChange={(v) => updateModelPrice(model, 'outputPerMillion', v)} /><Multiplier label="Cache 写" value={price.cacheCreationPerMillion} onChange={(v) => updateModelPrice(model, 'cacheCreationPerMillion', v)} /><Multiplier label="Cache 读" value={price.cacheReadPerMillion} onChange={(v) => updateModelPrice(model, 'cacheReadPerMillion', v)} /></div><Button variant="ghost" size="icon" className="shrink-0" onClick={() => removeModel(model)}><Trash2 className="size-4" /></Button></div>)}<Button variant="outline" size="sm" onClick={addModel}><Plus className="mr-1.5 size-4" />添加模型</Button></div></div>

      <div><h3 className="mb-3 text-sm font-semibold">上游凭据成本倍率</h3><div className="grid gap-2 sm:grid-cols-2 md:grid-cols-3">{upstreamCredentials.map((c) => <Multiplier key={c.id} label={c.email || `凭据 #${c.id}`} value={localConfig?.upstreamMultipliers?.[c.id] ?? 1} onChange={(v) => updateUpstreamMultiplier(c.id, v)} />)}</div></div>

      <div><h3 className="mb-3 text-sm font-semibold">引擎计费倍率</h3><div className="grid gap-2 sm:grid-cols-2 md:grid-cols-4"><Multiplier label="Rust 缓存" value={localConfig?.rustMultiplier ?? 1} onChange={(v) => updateEngineMultiplier('rust', v)} /><Multiplier label="Go 缓存" value={localConfig?.goMultiplier ?? 1} onChange={(v) => updateEngineMultiplier('go', v)} /><Multiplier label="真实上游" value={localConfig?.realMultiplier ?? 1} onChange={(v) => updateEngineMultiplier('real', v)} /><Multiplier label="无缓存" value={localConfig?.nocacheMultiplier ?? 1} onChange={(v) => updateEngineMultiplier('nocache', v)} /></div><p className="mt-2 text-xs text-muted-foreground">调节系数：用于计费对比表，不影响实际客户端计费。各引擎的 token 真实膨胀倍率已作用在 clientCost 上。</p></div>
    </CardContent></Card>
  </div>
}

function Multiplier({ label, value, onChange }: { label: string; value: number; onChange: (value: string) => void }) { return <label className="text-xs text-muted-foreground">{label}<Input className="mt-1" type="number" min="0" step="0.01" value={value} onChange={(e) => onChange(e.target.value)} /></label> }
