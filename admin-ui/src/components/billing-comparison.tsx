import { useEffect, useMemo, useState } from 'react'
import { Save, Plus, Trash2 } from 'lucide-react'
import { Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
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

export function BillingComparison({ timeFilter, statsFilter }: Props) {
  const { data: config } = useBillingConfig()
  const { data: credentials } = useCredentials()
  const { data: comparison } = useBillingComparison(timeFilter, statsFilter)
  const save = useSetBillingConfig()
  const [draft, setDraft] = useState(config)

  useEffect(() => { if (config) setDraft(config) }, [config])
  const upstream = useMemo(
    () => (credentials?.credentials ?? []).filter((item) => item.isUpstream),
    [credentials],
  )
  if (!draft) return null

  const models = Object.entries(draft.modelPrices)
  const updatePrice = (model: string, key: keyof ModelPricing, value: string) => {
    const next = Number(value)
    setDraft({ ...draft, modelPrices: { ...draft.modelPrices, [model]: { ...draft.modelPrices[model], [key]: Number.isFinite(next) ? next : 0 } } })
  }
  const addModel = () => {
    let name = 'claude-sonnet-4-5'
    let i = 2
    while (draft.modelPrices[name]) name = `model-${i++}`
    setDraft({ ...draft, modelPrices: { ...draft.modelPrices, [name]: emptyPrice() } })
  }
  const removeModel = (model: string) => {
    const prices = { ...draft.modelPrices }
    delete prices[model]
    setDraft({ ...draft, modelPrices: prices })
  }
  const setUpstreamMultiplier = (id: number, value: string) => {
    const n = Number(value)
    setDraft({ ...draft, upstreamMultipliers: { ...draft.upstreamMultipliers, [id]: Number.isFinite(n) ? n : 1 } })
  }

  return <div className="space-y-4">
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0">
        <div><CardTitle>费用对比</CardTitle><p className="mt-1 text-xs text-muted-foreground">美元 / 1M token，仅统计上游 API 账号</p></div>
        <Button size="sm" onClick={() => save.mutate(draft)} disabled={save.isPending}><Save className="mr-1.5 h-3.5 w-3.5" />保存配置</Button>
      </CardHeader>
      <CardContent>
        <div className="grid gap-3 sm:grid-cols-3">
          <Multiplier label="Rust 引擎倍率" value={draft.rustMultiplier} onChange={(v) => setDraft({ ...draft, rustMultiplier: Number(v) || 0 })} />
          <Multiplier label="Go 引擎倍率" value={draft.goMultiplier} onChange={(v) => setDraft({ ...draft, goMultiplier: Number(v) || 0 })} />
        </div>
      </CardContent>
    </Card>

    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0"><CardTitle>模型价格</CardTitle><Button size="sm" variant="outline" onClick={addModel}><Plus className="mr-1.5 h-3.5 w-3.5" />添加模型</Button></CardHeader>
      <CardContent className="overflow-x-auto">
        <table className="w-full min-w-[760px] text-sm"><thead><tr className="border-b text-left text-xs text-muted-foreground"><th className="pb-2 pr-3">模型</th><th className="pb-2 pr-3">输入</th><th className="pb-2 pr-3">输出</th><th className="pb-2 pr-3">缓存写入</th><th className="pb-2 pr-3">缓存读取</th><th /></tr></thead><tbody>
          {models.map(([model, price]) => <tr key={model} className="border-b last:border-0"><td className="py-2 pr-3"><Input value={model} onChange={(e) => { const prices = { ...draft.modelPrices }; prices[e.target.value] = prices[model]; delete prices[model]; setDraft({ ...draft, modelPrices: prices }) }} /></td>
            {(['inputPerMillion', 'outputPerMillion', 'cacheCreationPerMillion', 'cacheReadPerMillion'] as const).map((key) => <td key={key} className="py-2 pr-3"><Input type="number" min="0" step="0.0001" value={price[key]} onChange={(e) => updatePrice(model, key, e.target.value)} /></td>)}
            <td className="py-2"><Button size="icon" variant="ghost" title="删除模型" onClick={() => removeModel(model)}><Trash2 className="h-4 w-4" /></Button></td></tr>)}
          {models.length === 0 && <tr><td colSpan={6} className="py-6 text-center text-sm text-muted-foreground">尚未配置模型价格</td></tr>}
        </tbody></table>
      </CardContent>
    </Card>

    <Card><CardHeader><CardTitle>上游账号倍率</CardTitle></CardHeader><CardContent className="space-y-2">{upstream.length === 0 ? <p className="text-sm text-muted-foreground">暂无上游 API 账号</p> : upstream.map((item) => <div key={item.id} className="flex items-center gap-3"><div className="min-w-0 flex-1"><div className="text-sm">账号 #{item.id}</div><div className="truncate text-xs text-muted-foreground">{item.email || item.upstreamBaseUrl || '未命名上游账号'}</div></div><Input className="w-28" type="number" min="0" step="0.01" value={draft.upstreamMultipliers[String(item.id)] ?? 1} onChange={(e) => setUpstreamMultiplier(item.id, e.target.value)} /></div>)}</CardContent></Card>

    <Card><CardHeader><CardTitle>费用趋势</CardTitle></CardHeader><CardContent><div className="h-64"><ResponsiveContainer width="100%" height="100%"><LineChart data={comparison?.points ?? []}><XAxis dataKey="ts" hide /><YAxis tickFormatter={(v) => `$${v}`} /><Tooltip formatter={(v: number) => money(v)} /><Line type="monotone" dataKey="upstreamCost" name="上游真实" stroke="#ef4444" dot={false} /><Line type="monotone" dataKey="rustCost" name="Rust 模拟" stroke="#3b82f6" dot={false} /><Line type="monotone" dataKey="goCost" name="Go 模拟" stroke="#10b981" dot={false} /></LineChart></ResponsiveContainer></div></CardContent></Card>

    <div className="grid gap-3 sm:grid-cols-3">{[['上游真实', comparison?.upstreamCost ?? 0, comparison?.upstreamTokens ?? 0], ['Rust 模拟', comparison?.rustCost ?? 0, comparison?.rustTokens ?? 0], ['Go 模拟', comparison?.goCost ?? 0, comparison?.goTokens ?? 0]].map(([label, cost, tokens]) => <Card key={label as string}><CardContent className="pt-5"><div className="text-xs text-muted-foreground">{label}</div><div className="mt-1 text-2xl font-semibold">{money(cost as number)}</div><div className="mt-1 text-xs text-muted-foreground">{formatNumber(tokens as number)} tokens</div></CardContent></Card>)}</div>
  </div>
}

function Multiplier({ label, value, onChange }: { label: string; value: number; onChange: (value: string) => void }) { return <label className="text-xs text-muted-foreground">{label}<Input className="mt-1" type="number" min="0" step="0.01" value={value} onChange={(e) => onChange(e.target.value)} /></label> }

