import { ProxySelector } from '@/components/proxy-selector'
import type { ImportProxyChoice } from '@/lib/credential-proxy'

export function ImportProxyField({ value, onChange, disabled }: {
  value: ImportProxyChoice
  onChange: (value: ImportProxyChoice) => void
  disabled?: boolean
}) {
  return (
    <div className="space-y-2 rounded-lg border p-3">
      <label className="text-sm font-medium">导入凭证使用的 IP / 代理</label>
      <select className="h-9 w-full rounded-md border bg-background px-3 text-sm"
        aria-label="导入代理设置" disabled={disabled}
        value={value.kind === 'preserve' ? 'preserve' : 'override'}
        onChange={(event) => onChange(event.target.value === 'preserve' ? { kind: 'preserve' } : { kind: 'global' })}>
        <option value="preserve">保留文件中的代理设置</option>
        <option value="override">为本次导入统一指定代理</option>
      </select>
      {value.kind !== 'preserve' && <ProxySelector value={value} onChange={onChange} disabled={disabled} />}
      <p className="text-xs text-muted-foreground">
        {value.kind === 'preserve'
          ? '保留每条凭证的 proxyId / proxyUrl；文件未设置时使用全局代理，全局未配置则直连。不会随机分配代理。'
          : '所选设置覆盖文件中的代理，在首次令牌刷新和验活前生效。导入时代理连接失败会报错，不改用直连；后续自动轮换遵循 IP 管理设置。'}
      </p>
    </div>
  )
}
