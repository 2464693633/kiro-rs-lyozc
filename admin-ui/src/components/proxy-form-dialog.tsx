import { useMemo, useState } from 'react'
import { toast } from 'sonner'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select'
import { addProxy, batchAddProxies, updateProxy } from '@/api/credentials'
import { extractErrorMessage, cn } from '@/lib/utils'
import { PROXY_PROTOCOLS, hostPort, parseProxyUrl, proxyIdentity } from '@/components/proxy-format'
import type { ProxyPoolEntry, ProxyProtocol, UpdateProxyRequest } from '@/types/api'

/** 新建或改了地址后，后端在后台检测（8 路并发，单个最长约 8 秒），检测完再刷新一次列表 */
function useProbeRefresh() {
  const queryClient = useQueryClient()
  return (count: number) => {
    queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    if (count <= 0) return
    const delay = Math.min(Math.ceil(count / 8) * 9_000, 45_000)
    window.setTimeout(() => queryClient.invalidateQueries({ queryKey: ['proxy-pool'] }), delay)
  }
}

/** 表单状态；端口用字符串，便于输入时为空 */
interface ProxyForm {
  name: string
  protocol: ProxyProtocol
  host: string
  port: string
  username: string
  password: string
  /** 编辑时：清除已保存的密码 */
  clearPassword: boolean
}

const EMPTY_FORM: ProxyForm = {
  name: '',
  protocol: 'http',
  host: '',
  port: '',
  username: '',
  password: '',
  clearPassword: false,
}

/** 提交前的基本校验，完整校验在后端 */
function validateForm(form: ProxyForm): string | null {
  if (!form.host.trim()) return '请填写主机地址'
  const port = Number(form.port)
  if (!form.port.trim() || !Number.isInteger(port) || port < 1 || port > 65535) {
    return '端口必须在 1-65535 之间'
  }
  if (form.password && !form.username.trim()) return '填写密码时必须同时填写用户名'
  return null
}

interface ProxyFieldsProps {
  form: ProxyForm
  onChange: (patch: Partial<ProxyForm>) => void
  disabled?: boolean
  /** 编辑时：该代理已保存了密码（列表不回显密码本身） */
  storedPassword?: boolean
}

/** 名称、协议、主机、端口与认证字段；新建和编辑共用 */
function ProxyFields({ form, onChange, disabled, storedPassword = false }: ProxyFieldsProps) {
  const userCleared = storedPassword && !form.username.trim()

  // 往主机栏粘贴完整地址时自动拆分到各字段
  const handleHostPaste = (e: React.ClipboardEvent<HTMLInputElement>) => {
    const text = e.clipboardData.getData('text').trim()
    if (!text.includes('://')) return
    const parsed = parseProxyUrl(text)
    if (!parsed.ok) return
    e.preventDefault()
    const p = parsed.value
    onChange({
      protocol: p.protocol,
      host: p.host,
      port: String(p.port),
      username: p.username ?? '',
      password: p.password ?? '',
      clearPassword: storedPassword && !p.password,
    })
  }

  return (
    <div className="space-y-3">
      <div className="space-y-1">
        <label className="text-sm font-medium">名称</label>
        <Input
          value={form.name}
          onChange={(e) => onChange({ name: e.target.value })}
          placeholder={form.host.trim() ? `留空则为 ${form.host.trim()}:${form.port || '端口'}` : '留空则使用 主机:端口'}
          disabled={disabled}
          autoComplete="off"
        />
      </div>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-[120px_1fr_96px]">
        <div className="col-span-2 space-y-1 sm:col-span-1">
          <label className="text-sm font-medium">协议</label>
          <Select
            value={form.protocol}
            onValueChange={(v) => onChange({ protocol: v as ProxyProtocol })}
            disabled={disabled}
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PROXY_PROTOCOLS.map((p) => (
                <SelectItem key={p.value} value={p.value}>{p.label}</SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="space-y-1">
          <label className="text-sm font-medium">主机 *</label>
          <Input
            value={form.host}
            onChange={(e) => onChange({ host: e.target.value })}
            onPaste={handleHostPaste}
            placeholder="1.2.3.4 或 proxy.example.com"
            disabled={disabled}
            spellCheck={false}
            autoComplete="off"
            className="font-mono"
          />
        </div>
        <div className="space-y-1">
          <label className="text-sm font-medium">端口 *</label>
          <Input
            value={form.port}
            onChange={(e) => onChange({ port: e.target.value.replace(/\D/g, '').slice(0, 5) })}
            inputMode="numeric"
            placeholder="1080"
            disabled={disabled}
            autoComplete="off"
            className="font-mono"
          />
        </div>
      </div>
      <p className="text-xs text-muted-foreground">
        可直接把完整地址（如 socks5://user:pass@host:1080）粘贴到主机栏自动拆分。SOCKS5H / SOCKS4A 由代理解析域名。
      </p>
      <div className="grid grid-cols-2 gap-2">
        <div className="space-y-1">
          <label className="text-sm font-medium">用户名</label>
          <Input
            value={form.username}
            onChange={(e) => {
              const username = e.target.value
              // 清空用户名时密码也一并清除（后端不接受只有密码）
              onChange(storedPassword && !username.trim() ? { username, password: '' } : { username })
            }}
            placeholder="可选"
            disabled={disabled}
            spellCheck={false}
            autoComplete="off"
            data-lpignore="true"
            data-1p-ignore="true"
          />
        </div>
        <div className="space-y-1">
          <label className="text-sm font-medium">密码</label>
          <Input
            type="password"
            value={form.password}
            onChange={(e) => onChange({ password: e.target.value })}
            placeholder={
              userCleared ? '用户名已清空，密码将一并清除'
                : form.clearPassword ? '保存时清除密码'
                : storedPassword ? '已设置，留空则保留' : '可选'
            }
            disabled={disabled || userCleared || form.clearPassword}
            autoComplete="new-password"
            data-lpignore="true"
            data-1p-ignore="true"
          />
        </div>
      </div>
      {storedPassword && !userCleared && (
        <label className="flex w-fit cursor-pointer items-center gap-2 text-xs text-muted-foreground">
          <Checkbox
            checked={form.clearPassword}
            onCheckedChange={(v) => onChange({ clearPassword: v === true, password: '' })}
            disabled={disabled}
          />
          清除已保存的密码
        </label>
      )}
    </div>
  )
}

interface BatchLine {
  line: number
  status: 'valid' | 'duplicate' | 'invalid'
  error?: string
}

/** 逐行预检批量导入的地址；空行与 # 开头的行跳过，行号与后端报错一致 */
function analyzeBatch(text: string, existing: ProxyPoolEntry[]): BatchLine[] {
  // 列表不回显密码，只能按「地址 + 是否有密码」判断与已有代理重复
  const known = new Set(existing.map((p) => `${proxyIdentity(p)}|${p.hasPassword}`))
  const seen = new Set<string>()
  const result: BatchLine[] = []
  text.split('\n').forEach((raw, i) => {
    const line = raw.trim()
    if (!line || line.startsWith('#')) return
    const parsed = parseProxyUrl(line)
    if (!parsed.ok) {
      result.push({ line: i + 1, status: 'invalid', error: parsed.error })
      return
    }
    const p = parsed.value
    const exact = `${proxyIdentity(p)}|${p.password ?? ''}`
    const dup = seen.has(exact) || known.has(`${proxyIdentity(p)}|${Boolean(p.password)}`)
    seen.add(exact)
    result.push({ line: i + 1, status: dup ? 'duplicate' : 'valid' })
  })
  return result
}

/** 批量导入预检结果；提交后改为显示后端返回的跳过原因 */
function BatchPreview({ lines, serverErrors }: { lines: BatchLine[]; serverErrors: string[] }) {
  if (serverErrors.length > 0) {
    return (
      <div className="max-h-32 space-y-0.5 overflow-y-auto rounded-md border p-2 text-xs">
        <div className="font-medium text-amber-600 dark:text-amber-400">未导入的行：</div>
        {serverErrors.map((msg, i) => <div key={i} className="text-muted-foreground">{msg}</div>)}
      </div>
    )
  }
  if (lines.length === 0) return null
  const count = (s: BatchLine['status']) => lines.filter((l) => l.status === s).length
  const invalid = lines.filter((l) => l.status === 'invalid')
  return (
    <div className="space-y-1 text-xs">
      <div className="flex flex-wrap gap-3">
        <span className="text-emerald-600 dark:text-emerald-400">可导入 {count('valid')}</span>
        <span
          className="text-amber-600 dark:text-amber-400"
          title="协议、用户名、主机、端口与已有代理或前面的行相同；列表不显示密码，密码不同的仍可导入"
        >
          重复 {count('duplicate')}
        </span>
        <span className="text-destructive">格式错误 {invalid.length}</span>
      </div>
      {invalid.length > 0 && (
        <div className="max-h-24 space-y-0.5 overflow-y-auto rounded-md border p-2 text-muted-foreground">
          {invalid.map((l) => <div key={l.line}>第 {l.line} 行：{l.error}</div>)}
        </div>
      )}
    </div>
  )
}

interface CreateProxyDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 已有代理，用于批量导入时预检重复 */
  existing: ProxyPoolEntry[]
}

/** 新建代理：单个填写结构化字段，或批量粘贴地址（每行一个） */
export function CreateProxyDialog({ open, onOpenChange, existing }: CreateProxyDialogProps) {
  const [mode, setMode] = useState<'single' | 'batch'>('single')
  const [form, setForm] = useState<ProxyForm>(EMPTY_FORM)
  const [batchText, setBatchText] = useState('')
  const [batchErrors, setBatchErrors] = useState<string[]>([])
  const refreshAfterProbe = useProbeRefresh()
  const patchForm = (patch: Partial<ProxyForm>) => setForm((f) => ({ ...f, ...patch }))

  const lines = useMemo(() => analyzeBatch(batchText, existing), [batchText, existing])
  const importable = lines.filter((l) => l.status !== 'invalid').length

  const addOne = useMutation({
    mutationFn: () =>
      addProxy({
        name: form.name.trim() || undefined,
        protocol: form.protocol,
        host: form.host.trim(),
        port: Number(form.port),
        username: form.username.trim() || undefined,
        password: form.password || undefined,
      }),
    onSuccess: (entry) => {
      toast.success(`已添加代理 ${entry.name}，正在后台检测连通性`)
      refreshAfterProbe(1)
      onOpenChange(false)
    },
    onError: (err) => toast.error(`添加失败: ${extractErrorMessage(err)}`),
  })

  // 原样按行提交，后端报错的行号与输入框一致
  const addBatch = useMutation({
    mutationFn: () => batchAddProxies({ urls: batchText.split('\n') }),
    onSuccess: (res) => {
      refreshAfterProbe(res.added)
      if (res.errors === 0) {
        toast.success(`已导入 ${res.added} 个代理，正在后台检测连通性`)
        onOpenChange(false)
        return
      }
      if (res.added > 0) toast.warning(`已导入 ${res.added} 个，${res.errors} 行未导入`)
      else toast.error(`${res.errors} 行均未导入`)
      setBatchErrors(res.errorMessages)
    },
    onError: (err) => toast.error(`导入失败: ${extractErrorMessage(err)}`),
  })

  const pending = addOne.isPending || addBatch.isPending

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    if (mode === 'batch') {
      if (importable === 0) {
        toast.error('没有可导入的地址')
        return
      }
      addBatch.mutate()
      return
    }
    const error = validateForm(form)
    if (error) {
      toast.error(error)
      return
    }
    addOne.mutate()
  }

  return (
    <Dialog open={open} onOpenChange={(v) => { if (!pending) onOpenChange(v) }}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>添加代理</DialogTitle>
          <DialogDescription>保存后可在添加或编辑账号时直接选择；添加后自动检测连通性与出口位置。</DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-4" autoComplete="off">
          <div className="inline-flex rounded-lg border p-0.5 text-[13px]">
            {(['single', 'batch'] as const).map((m) => (
              <button
                key={m}
                type="button"
                onClick={() => setMode(m)}
                disabled={pending}
                className={cn(
                  'rounded-md px-3 py-1 transition-colors',
                  mode === m ? 'bg-primary/12 font-medium text-foreground' : 'text-muted-foreground hover:text-foreground',
                )}
              >
                {m === 'single' ? '单个添加' : '批量导入'}
              </button>
            ))}
          </div>
          {mode === 'single' ? (
            <ProxyFields form={form} onChange={patchForm} disabled={pending} />
          ) : (
            <div className="space-y-2">
              <Textarea
                value={batchText}
                onChange={(e) => {
                  setBatchText(e.target.value)
                  setBatchErrors([])
                }}
                placeholder={'每行一个地址，# 开头为注释\nsocks5://user:pass@1.2.3.4:1080\nhttp://5.6.7.8:8080'}
                disabled={pending}
                spellCheck={false}
                className="min-h-[160px] font-mono text-xs"
              />
              <BatchPreview lines={lines} serverErrors={batchErrors} />
            </div>
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={pending}>
              取消
            </Button>
            <Button type="submit" disabled={pending || (mode === 'batch' && importable === 0)}>
              {pending ? '提交中…' : mode === 'batch' ? '导入' : '添加'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

/** 只提交改动过的字段；省略的字段后端不修改 */
function buildUpdate(form: ProxyForm, proxy: ProxyPoolEntry): UpdateProxyRequest {
  const req: UpdateProxyRequest = {}
  const host = form.host.trim().replace(/^\[|\]$/g, '').toLowerCase()
  const port = Number(form.port)
  const username = form.username.trim()
  if (form.protocol !== proxy.protocol) req.protocol = form.protocol
  if (host !== proxy.host) req.host = host
  if (port !== proxy.port) req.port = port
  if (username !== (proxy.username ?? '')) req.username = username
  if (form.password) req.password = form.password
  // 清除密码，或清空了用户名（后端不接受只有密码）
  else if (proxy.hasPassword && (form.clearPassword || !username)) req.password = ''
  // 名称留空表示用 主机:端口；地址变了要按新地址重算
  const name = form.name.trim()
  const oldName = proxy.name === hostPort(proxy.host, proxy.port) ? '' : proxy.name
  if (name !== oldName || (!name && (req.host !== undefined || req.port !== undefined))) req.name = name
  return req
}

interface EditProxyDialogProps {
  proxy: ProxyPoolEntry
  /** 该代理当前是全局代理 */
  isGlobal: boolean
  onOpenChange: (open: boolean) => void
}

const ENDPOINT_FIELDS = ['protocol', 'host', 'port', 'username', 'password'] as const

/** 编辑代理：地址或认证变化时，后端同步到所有关联账号并重新检测 */
export function EditProxyDialog({ proxy, isGlobal, onOpenChange }: EditProxyDialogProps) {
  const queryClient = useQueryClient()
  const refreshAfterProbe = useProbeRefresh()
  const [form, setForm] = useState<ProxyForm>(() => ({
    ...EMPTY_FORM,
    name: proxy.name === hostPort(proxy.host, proxy.port) ? '' : proxy.name,
    protocol: proxy.protocol,
    host: proxy.host,
    port: String(proxy.port),
    username: proxy.username ?? '',
  }))
  const patchForm = (patch: Partial<ProxyForm>) => setForm((f) => ({ ...f, ...patch }))

  const save = useMutation({
    mutationFn: (req: UpdateProxyRequest) => updateProxy(proxy.id, req),
    onSuccess: (entry, req) => {
      if (ENDPOINT_FIELDS.some((k) => req[k] !== undefined)) {
        refreshAfterProbe(1)
        toast.success(
          proxy.credentialCount > 0
            ? `已保存，已同步到 ${proxy.credentialCount} 个关联账号，正在重新检测`
            : '已保存，正在重新检测',
        )
      } else {
        queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
        toast.success(`已保存 ${entry.name}`)
      }
      // 账号列表会显示代理名称
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      onOpenChange(false)
    },
    onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
  })

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    const error = validateForm(form)
    if (error) {
      toast.error(error)
      return
    }
    const req = buildUpdate(form, proxy)
    if (Object.keys(req).length === 0) {
      onOpenChange(false)
      return
    }
    save.mutate(req)
  }

  return (
    <Dialog open onOpenChange={(v) => { if (!save.isPending) onOpenChange(v) }}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>编辑代理 #{proxy.id}</DialogTitle>
          <DialogDescription>改动地址或认证会同步到所有关联账号，并重新检测连通性。</DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-4" autoComplete="off">
          <ProxyFields
            form={form}
            onChange={patchForm}
            disabled={save.isPending}
            storedPassword={proxy.hasPassword}
          />
          {proxy.credentialCount > 0 && (
            <p className="text-xs text-amber-600 dark:text-amber-400">
              当前有 {proxy.credentialCount} 个账号使用此代理，改动地址或认证后它们立即改用新配置。
            </p>
          )}
          {isGlobal && (
            <p className="text-xs text-amber-600 dark:text-amber-400">
              此代理是当前的全局代理。全局代理单独保存，改了地址或认证后请重新「设为全局」。
            </p>
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={save.isPending}>
              取消
            </Button>
            <Button type="submit" disabled={save.isPending}>
              {save.isPending ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
