import { CheckCircle2, HelpCircle, XCircle } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import type { ProxyExitInfo, ProxyPoolEntry, ProxyProtocol } from '@/types/api'

/** 新建/编辑代理时可选的协议 */
export const PROXY_PROTOCOLS: { value: ProxyProtocol; label: string }[] = [
  { value: 'http', label: 'HTTP' },
  { value: 'https', label: 'HTTPS' },
  { value: 'socks5', label: 'SOCKS5' },
  { value: 'socks5h', label: 'SOCKS5H' },
  { value: 'socks4', label: 'SOCKS4' },
  { value: 'socks4a', label: 'SOCKS4A' },
]

const PROTOCOL_VALUES = PROXY_PROTOCOLS.map((p) => p.value) as string[]

/** 未写端口时的默认端口；socks 系列必须写端口（与后端一致） */
const DEFAULT_PORTS: Partial<Record<ProxyProtocol, number>> = { http: 80, https: 443 }

/** 解析后的代理地址 */
export interface ParsedProxy {
  protocol: ProxyProtocol
  host: string
  port: number
  username?: string
  password?: string
}

/** `主机:端口`，IPv6 主机加方括号 */
export function hostPort(host: string, port: number): string {
  return host.includes(':') ? `[${host}]:${port}` : `${host}:${port}`
}

function decode(s: string): string {
  try {
    return decodeURIComponent(s)
  } catch {
    return s
  }
}

/**
 * 解析 `协议://[用户名[:密码]@]主机[:端口]`，规则与后端一致，用于批量导入预览和粘贴拆分。
 * 后端仍会再校验一次。
 */
export function parseProxyUrl(raw: string): { ok: true; value: ParsedProxy } | { ok: false; error: string } {
  const m = raw.trim().match(/^([a-zA-Z][a-zA-Z0-9+.-]*):\/\/(?:([^/?#]*)@)?(\[[^\]]*\]|[^:/?#]*)(?::(\d*))?(?:[/?#].*)?$/)
  if (!m) return { ok: false, error: '格式无效，应为 协议://[用户名:密码@]主机:端口' }
  const [, scheme, userinfo, rawHost, rawPort] = m
  const protocol = scheme.toLowerCase() as ProxyProtocol
  if (!PROTOCOL_VALUES.includes(protocol)) {
    return { ok: false, error: `不支持的协议 ${scheme}` }
  }
  const bracketed = rawHost.startsWith('[')
  const host = (bracketed ? rawHost.slice(1, -1) : rawHost).toLowerCase()
  if (!host) return { ok: false, error: '缺少主机' }
  if (/\s/.test(host) || (bracketed ? !/^[0-9a-f:.]+$/.test(host) : host.includes(':'))) {
    return { ok: false, error: '主机格式无效' }
  }
  const port = rawPort ? Number(rawPort) : DEFAULT_PORTS[protocol]
  if (port === undefined) return { ok: false, error: '缺少端口号' }
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    return { ok: false, error: '端口必须在 1-65535 之间' }
  }
  let username: string | undefined
  let password: string | undefined
  if (userinfo !== undefined) {
    const sep = userinfo.indexOf(':')
    username = decode(sep >= 0 ? userinfo.slice(0, sep) : userinfo).trim() || undefined
    password = sep >= 0 ? decode(userinfo.slice(sep + 1)).trim() || undefined : undefined
  }
  if (password && !username) return { ok: false, error: '填写密码时必须同时填写用户名' }
  return { ok: true, value: { protocol, host, port, username, password } }
}

/** 不含密码的地址身份，用于预览时判断是否与已有代理重复（列表不回显密码） */
export function proxyIdentity(p: { protocol: string; host: string; port: number; username?: string }): string {
  return `${p.protocol}://${p.username ? `${p.username}@` : ''}${hostPort(p.host, p.port)}`
}

/** 国家代码转国旗 emoji，如 JP → 🇯🇵 */
export function flagEmoji(countryCode?: string): string {
  const code = countryCode?.trim().toUpperCase() ?? ''
  if (!/^[A-Z]{2}$/.test(code)) return ''
  return String.fromCodePoint(...[...code].map((c) => 0x1f1e6 + c.charCodeAt(0) - 65))
}

/** 出口位置：国旗 + 城市 / 国家；只有 IP 时返回空串 */
export function formatLocation(exit?: ProxyExitInfo): string {
  if (!exit) return ''
  const place = [exit.city, exit.country].filter(Boolean).join(' · ')
  return [flagEmoji(exit.countryCode), place].filter(Boolean).join(' ')
}

/** 健康状态徽标：可用显示延迟，异常显示连续失败次数，悬停查看失败原因 */
export function ProxyHealthBadge({ proxy }: { proxy: ProxyPoolEntry }) {
  if (proxy.health === 'healthy') {
    return (
      <Badge
        variant="outline"
        className="gap-1 border-emerald-500/50 text-[11px] text-emerald-600 dark:text-emerald-400"
      >
        <CheckCircle2 className="h-3 w-3" />
        {proxy.latencyMs != null ? `${proxy.latencyMs}ms` : '可用'}
      </Badge>
    )
  }
  if (proxy.health === 'unhealthy') {
    return (
      <Badge
        variant="outline"
        className="gap-1 border-destructive/50 text-[11px] text-destructive"
        title={proxy.lastError}
      >
        <XCircle className="h-3 w-3" />
        异常{proxy.consecutiveFailures > 0 ? ` ×${proxy.consecutiveFailures}` : ''}
      </Badge>
    )
  }
  return (
    <Badge variant="outline" className="gap-1 text-[11px] text-muted-foreground">
      <HelpCircle className="h-3 w-3" />
      未检测
    </Badge>
  )
}
