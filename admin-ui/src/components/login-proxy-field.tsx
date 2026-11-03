import { ProxySelector, type ProxyChoice } from '@/components/proxy-selector'

export function LoginProxyField({ value, onChange, disabled }: {
  value: ProxyChoice
  onChange: (value: ProxyChoice) => void
  disabled?: boolean
}) {
  return (
    <div className="space-y-1.5">
      <label className="text-sm font-medium">登录与凭证使用的 IP / 代理</label>
      <ProxySelector value={value} onChange={onChange} disabled={disabled} />
      <p className="text-xs text-muted-foreground">
        服务器从首次授权请求起使用此设置，登录成功后保存到凭证。浏览器授权页面使用本机网络，如需同一出口，请在浏览器端配置对应代理。
      </p>
      <p className="text-xs text-muted-foreground">如需长期固定 IP，请在「IP 管理」关闭自动轮换和直连回退。</p>
    </div>
  )
}
