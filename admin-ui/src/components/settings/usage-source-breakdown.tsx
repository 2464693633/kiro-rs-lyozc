import { useUsageSourceStats } from '@/hooks/use-traces'
import { extractErrorMessage } from '@/lib/utils'

/** 本地计量生效范围：只有走本地拆分的请求才受固定上报与读取目标影响。 */
const BUCKETS = [
  {
    key: 'adjusted',
    label: '已调整',
    hint: '走了本地拆分，并应用了输入扣减或固定上报口径',
    cls: 'bg-violet-500',
    local: true,
  },
  {
    key: 'simulated',
    label: '本地估算',
    hint: '上游未下发精确用量，按客户端 cache_control 断点在本地模拟',
    cls: 'bg-amber-500',
    local: true,
  },
  {
    key: 'provider',
    label: '上游真值',
    hint: '用量来自 Kiro metadataEvent.tokenUsage；固定上报与读取目标对这些请求不生效',
    cls: 'bg-sky-500',
    local: false,
  },
  {
    key: 'none',
    label: '无断点',
    hint: '请求未声明 cache_control 或该 Key 关闭了计量，输入全额计入 input_tokens',
    cls: 'bg-muted-foreground/40',
    local: false,
  },
  {
    key: 'unknown',
    label: '无记录',
    hint: '老记录或早退请求没有记录来源',
    cls: 'bg-border',
    local: false,
  },
] as const

export function UsageSourceBreakdown() {
  const query = useUsageSourceStats(86400)
  const stats = query.data

  if (query.isError) {
    return (
      <p className="text-[11px] text-muted-foreground" role="status">
        生效范围读取失败：{extractErrorMessage(query.error)}
      </p>
    )
  }
  if (!stats) {
    return <p className="text-[11px] text-muted-foreground">正在统计生效范围…</p>
  }
  if (stats.total === 0) {
    return (
      <p className="text-[11px] text-muted-foreground">
        最近 24 小时没有请求记录，暂时无法判断生效范围。请求日志关闭时这里也会为空。
      </p>
    )
  }

  const pct = (value: number) => (value / stats.total) * 100
  const localTotal = stats.adjusted + stats.simulated
  const present = BUCKETS.filter((bucket) => stats[bucket.key] > 0)

  return (
    <div className="space-y-2">
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-[12px] text-muted-foreground">
          本地计量生效范围（最近 24 小时）
        </span>
        <span className="text-[13px] font-medium tabular-nums">
          {pct(localTotal).toFixed(1)}%
        </span>
      </div>

      <div
        className="flex h-2 w-full overflow-hidden rounded-full bg-muted"
        role="img"
        aria-label={`共 ${stats.total} 条请求，其中 ${localTotal} 条走本地拆分`}
      >
        {present.map((bucket) => (
          <div
            key={bucket.key}
            className={bucket.cls}
            style={{ width: `${pct(stats[bucket.key])}%` }}
            title={`${bucket.label}：${stats[bucket.key]} 条（${pct(stats[bucket.key]).toFixed(1)}%）`}
          />
        ))}
      </div>

      <dl className="flex flex-wrap gap-x-4 gap-y-1">
        {present.map((bucket) => (
          <div key={bucket.key} className="flex items-center gap-1.5" title={bucket.hint}>
            <span className={`h-2 w-2 shrink-0 rounded-full ${bucket.cls}`} aria-hidden />
            <dt className="text-[11px] text-muted-foreground">{bucket.label}</dt>
            <dd className="text-[11px] tabular-nums">
              {stats[bucket.key].toLocaleString()}
              <span className="ml-1 text-muted-foreground">
                {pct(stats[bucket.key]).toFixed(1)}%
              </span>
            </dd>
          </div>
        ))}
      </dl>

      <p className="text-[11px] leading-relaxed text-muted-foreground">
        固定上报输入 Token 与 Key 级的模拟缓存读取目标 / 上限，只对「已调整」和「本地估算」
        这两类请求生效，共 {localTotal.toLocaleString()} 条。「上游真值」的用量由 Kiro 下发，
        本地配置不改写它；「无断点」的请求输入全额计入 input_tokens。
        {localTotal === 0 && ' 当前没有任何请求走本地拆分，调整这些设置不会改变上报数字。'}
      </p>
    </div>
  )
}
