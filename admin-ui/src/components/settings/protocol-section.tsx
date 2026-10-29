import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { CheckCircle2, Loader2, RefreshCw, Save } from 'lucide-react'
import { getProtocolCompatibility, setProtocolCompatibility } from '@/api/protocol-compatibility'
import { SettingGroup, SettingRow } from '@/components/console/setting-row'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Switch } from '@/components/ui/switch'
import { useClientKeys } from '@/hooks/use-client-keys'
import { extractErrorMessage } from '@/lib/utils'
import type { ProtocolCompatibilityConfig, ProtocolCompatibilityResponse } from '@/types/api'

const QUERY_KEY = ['protocol-compatibility']

interface Draft {
  codeExecutionMode: ProtocolCompatibilityConfig['codeExecutionMode']
  searchIncrementalStreaming: boolean
  signatureMetadataCompat: boolean
  scope: 'all' | 'selected'
  keyIds: string
}

function toDraft(config: ProtocolCompatibilityConfig): Draft {
  return {
    codeExecutionMode: config.codeExecutionMode,
    searchIncrementalStreaming: config.searchIncrementalStreaming,
    signatureMetadataCompat: config.signatureMetadataCompat ?? false,
    scope: config.enabledKeyIds.length ? 'selected' : 'all',
    keyIds: config.enabledKeyIds.join(', '),
  }
}

function parseKeyIds(raw: string): { ids: number[]; error?: string } {
  const values = raw.trim().split(/[\s,，;；]+/).filter(Boolean)
  if (values.some((value) => !/^\d+$/.test(value) || !Number.isSafeInteger(Number(value)) || Number(value) <= 0)) {
    return { ids: [], error: 'Key ID 必须是大于 0 的整数，多个 ID 用逗号或空格分隔。' }
  }
  return { ids: [...new Set(values.map(Number))].sort((a, b) => a - b) }
}

function modeLabel(mode: ProtocolCompatibilityConfig['codeExecutionMode']) {
  return mode === 'bridge' ? '工具结果桥接（bridge）' : '拒绝不支持的请求（reject）'
}

function scopeLabel(ids: number[], keys: { id: number; name: string }[]) {
  if (!ids.length) return '全部客户端 Key'
  return ids.map((id) => {
    const key = keys.find((item) => item.id === id)
    return key ? `${key.name}（#${id}）` : `Key #${id}`
  }).join('、')
}

function diagnosticLabel(value: string | undefined, expected: string, label: string) {
  return value === expected ? label : value || '未返回'
}

export function ProtocolSection() {
  const queryClient = useQueryClient()
  const query = useQuery({ queryKey: QUERY_KEY, queryFn: getProtocolCompatibility, retry: 1 })
  const keysQuery = useClientKeys()
  const mutation = useMutation({ mutationFn: setProtocolCompatibility })
  const [draft, setDraft] = useState<Draft | null>(null)
  const [dirty, setDirty] = useState(false)
  const [saveError, setSaveError] = useState('')
  const [saved, setSaved] = useState(false)
  const actual = query.data
  const keys = keysQuery.data?.keys ?? []

  useEffect(() => {
    if (query.data && !dirty) setDraft(toDraft(query.data))
  }, [query.data, dirty])

  const edit = (patch: Partial<Draft>) => {
    setDraft((current) => current ? { ...current, ...patch } : current)
    setDirty(true)
    setSaved(false)
    setSaveError('')
  }

  const parsedIds = parseKeyIds(draft?.keyIds ?? '')
  const scopeError = draft?.scope === 'selected'
    ? parsedIds.error || (!parsedIds.ids.length ? '请至少选择或输入一个 Key ID。' : '')
    : draft?.signatureMetadataCompat ? '签名元数据实验需要选择指定 Key。' : ''
  const disabled = !actual || !draft || mutation.isPending

  const save = async () => {
    if (!draft || scopeError) return
    setSaveError('')
    setSaved(false)
    try {
      const returned = await mutation.mutateAsync({
        codeExecutionMode: draft.codeExecutionMode,
        searchIncrementalStreaming: draft.searchIncrementalStreaming,
        signatureMetadataCompat: draft.signatureMetadataCompat,
        enabledKeyIds: draft.scope === 'all' ? [] : parsedIds.ids,
      })
      queryClient.setQueryData<ProtocolCompatibilityResponse>(QUERY_KEY, returned)
      setDraft(toDraft(returned))
      setDirty(false)
      setSaved(true)
    } catch (error) {
      setSaveError(`保存失败：${extractErrorMessage(error)}。草稿已保留，可以重试。`)
    }
  }

  const resetDraft = () => {
    if (!actual) return
    setDraft(toDraft(actual))
    setDirty(false)
    setSaved(false)
    setSaveError('')
  }

  const toggleKey = (id: number, checked: boolean) => {
    if (parsedIds.error) return
    const next = new Set(parsedIds.ids)
    if (checked) next.add(id)
    else next.delete(id)
    edit({ keyIds: [...next].sort((a, b) => a - b).join(', ') })
  }

  const diagnostics = actual?.diagnostics
  const version = actual?.moduleVersion || diagnostics?.moduleVersion
  const updatedAt = actual?.updatedAt ? new Date(actual.updatedAt) : null

  return (
    <div className="space-y-6">
      <SettingGroup
        title="协议兼容"
        description="设置工具结果与搜索响应的兼容策略，保存后按所选客户端 Key 范围生效。"
      >
        <div className="flex flex-wrap items-center justify-between gap-2 py-3">
          <p className="text-xs text-muted-foreground" role="status" aria-live="polite">
            {query.isLoading ? '正在读取服务器配置…' : dirty ? '有未保存的修改' : actual ? '已读取服务器配置' : '配置尚未加载'}
          </p>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => void query.refetch()}
            disabled={query.isFetching || mutation.isPending}
          >
            {query.isFetching ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="h-3.5 w-3.5" />}
            刷新生效值
          </Button>
        </div>
        {query.isError && (
          <p role="alert" className="rounded-md border border-destructive/30 bg-destructive/5 p-3 text-xs text-destructive">
            读取配置失败：{extractErrorMessage(query.error)}。请刷新重试{actual ? '，未保存的草稿不会被覆盖' : '，加载成功前不能保存'}。
          </p>
        )}
        <fieldset disabled={disabled} className="min-w-0 disabled:opacity-60">
          <SettingRow
            label="服务端代码执行"
            hint={draft?.codeExecutionMode === 'bridge'
              ? '通过工具调用和工具结果表达不可用状态。当前没有执行后端，不会运行代码，也不会返回虚构的执行成功。'
              : '遇到不支持的服务端代码执行请求时，直接返回不支持错误。默认使用此模式。'}
          >
            <Select
              value={draft?.codeExecutionMode ?? 'reject'}
              onValueChange={(value) => edit({ codeExecutionMode: value as Draft['codeExecutionMode'] })}
              disabled={disabled}
            >
              <SelectTrigger aria-label="服务端代码执行模式" className="w-full sm:w-64"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="reject">拒绝不支持的请求（reject）</SelectItem>
                <SelectItem value="bridge">工具结果桥接（bridge）</SelectItem>
              </SelectContent>
            </Select>
          </SettingRow>
          <SettingRow
            label="搜索增量流式输出"
            hint="流式搜索响应中逐段发送可用正文，便于客户端显示。实际等待时间仍取决于上游；默认关闭。"
          >
            <Switch
              checked={draft?.searchIncrementalStreaming ?? false}
              onCheckedChange={(value) => edit({ searchIncrementalStreaming: value })}
              disabled={disabled}
              aria-label="搜索增量流式输出"
            />
          </SettingRow>
          <SettingRow
            label="签名元数据实验"
            hint="按已观察到的对照样本调整签名外层格式，用于比较检测得分。仅支持指定 Key；未知格式原样返回，关闭后恢复透传。此实验不验证签名真实性。"
          >
            <Switch
              checked={draft?.signatureMetadataCompat ?? false}
              onCheckedChange={(value) => edit({ signatureMetadataCompat: value, ...(value ? { scope: 'selected' as const } : {}) })}
              disabled={disabled}
              aria-label="签名元数据实验"
            />
          </SettingRow>
          <SettingRow
            label="应用范围"
            hint="选择指定 Key 可先做小范围测试。范围外的请求保持默认策略；这里仅显示 Key 名称和 ID。"
          >
            <Select
              value={draft?.scope ?? 'all'}
              onValueChange={(value) => edit({ scope: value as Draft['scope'] })}
              disabled={disabled}
            >
              <SelectTrigger aria-label="协议兼容应用范围" className="w-full sm:w-64"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="all" disabled={draft?.signatureMetadataCompat}>全部客户端 Key</SelectItem>
                <SelectItem value="selected">仅指定 Key</SelectItem>
              </SelectContent>
            </Select>
          </SettingRow>
          {draft?.scope === 'selected' && (
            <div className="space-y-3 border-b border-border/50 py-3">
              <div className="max-h-52 overflow-y-auto rounded-md border border-border/60 p-3">
                {keysQuery.isLoading && <p className="text-xs text-muted-foreground">正在读取 Key 列表…</p>}
                {keysQuery.isError && <p className="text-xs text-destructive">Key 列表读取失败，可在下方手动输入 ID。</p>}
                {!keysQuery.isLoading && !keysQuery.isError && !keys.length && <p className="text-xs text-muted-foreground">暂无客户端 Key，可在客户端 Key 页面创建，或在下方手动输入 ID。</p>}
                {keys.length > 0 && (
                  <div className="grid gap-3 sm:grid-cols-2">
                    {keys.map((key) => (
                      <label key={key.id} className="flex min-w-0 items-center gap-2 text-xs">
                        <Checkbox
                          checked={parsedIds.ids.includes(key.id)}
                          onCheckedChange={(checked) => toggleKey(key.id, checked === true)}
                          disabled={disabled || !!parsedIds.error}
                          aria-label={`选择 ${key.name}，Key ID ${key.id}`}
                        />
                        <span className="min-w-0 break-all">{key.name} <span className="text-muted-foreground">#{key.id}{key.disabled ? ' · 已禁用' : ''}</span></span>
                      </label>
                    ))}
                  </div>
                )}
              </div>
              <div className="space-y-1.5">
                <label htmlFor="protocol-key-ids" className="text-xs font-medium">指定 Key ID</label>
                <Input
                  id="protocol-key-ids"
                  value={draft.keyIds}
                  onChange={(event) => edit({ keyIds: event.target.value })}
                  placeholder="例如：2, 5, 8"
                  autoComplete="off"
                  spellCheck={false}
                  aria-invalid={!!scopeError}
                  aria-describedby="protocol-key-ids-help"
                  className="font-mono text-xs"
                />
                <p id="protocol-key-ids-help" className={`text-xs ${scopeError ? 'text-destructive' : 'text-muted-foreground'}`}>
                  {scopeError || '多个 ID 用逗号或空格分隔，重复 ID 保存时自动合并。'}
                </p>
              </div>
            </div>
          )}
        </fieldset>
        <div className="flex flex-wrap items-center gap-2 pt-4">
          <Button type="button" size="sm" onClick={() => void save()} disabled={disabled || query.isFetching || !dirty || !!scopeError}>
            {mutation.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Save className="h-3.5 w-3.5" />}
            {mutation.isPending ? '保存中…' : '保存协议设置'}
          </Button>
          <Button type="button" size="sm" variant="outline" onClick={resetDraft} disabled={disabled || !dirty}>撤回修改</Button>
          {saved && <span role="status" className="inline-flex items-center gap-1 text-xs text-emerald-600"><CheckCircle2 className="h-3.5 w-3.5" />已保存，生效值已更新</span>}
        </div>
        {saveError && <p role="alert" className="mt-3 text-xs text-destructive">{saveError}</p>}
      </SettingGroup>

      <SettingGroup title="当前生效值" description="以下内容来自服务器最近一次 GET 或保存响应，未保存的草稿不会出现在这里。">
        {!actual ? <p className="py-3 text-xs text-muted-foreground">等待服务器返回配置。</p> : (
          <dl className="grid gap-3 py-3 text-xs sm:grid-cols-2">
            <div><dt className="text-muted-foreground">代码执行模式</dt><dd className="mt-1 font-medium">{modeLabel(actual.codeExecutionMode)}</dd></div>
            <div><dt className="text-muted-foreground">搜索增量流式输出</dt><dd className="mt-1 font-medium">{actual.searchIncrementalStreaming ? '开启' : '关闭'}</dd></div>
            <div><dt className="text-muted-foreground">签名元数据实验</dt><dd className="mt-1 font-medium">{actual.signatureMetadataCompat && actual.enabledKeyIds.length ? '开启（仅指定 Key）' : '关闭（原样透传）'}</dd></div>
            <div className="sm:col-span-2"><dt className="text-muted-foreground">应用范围</dt><dd className="mt-1 break-all font-medium">{scopeLabel(actual.enabledKeyIds, keys)}</dd></div>
            {updatedAt && !Number.isNaN(updatedAt.getTime()) && <div className="sm:col-span-2"><dt className="text-muted-foreground">服务器更新时间</dt><dd className="mt-1">{updatedAt.toLocaleString()}</dd></div>}
          </dl>
        )}
      </SettingGroup>

      <SettingGroup title="能力诊断" description="显示当前模块的实际能力，配置开关不会增加上游不具备的能力。">
        <dl className="grid gap-4 py-3 text-xs sm:grid-cols-2">
          <div><dt className="text-muted-foreground">代码执行后端</dt><dd className="mt-1"><Badge variant="outline">{diagnosticLabel(diagnostics?.codeExecutionBackend, 'unavailable', '未接入执行后端')}</Badge></dd></div>
          <div><dt className="text-muted-foreground">系统提示传输</dt><dd className="mt-1">{diagnosticLabel(diagnostics?.systemTransport, 'conversation_history', '通过对话历史传递')}</dd></div>
          <div><dt className="text-muted-foreground">Thinking 签名跨渠道重放</dt><dd className="mt-1">{diagnosticLabel(diagnostics?.signatureReplay, 'unsupported', '不支持')}</dd></div>
          {version && <div><dt className="text-muted-foreground">兼容模块版本</dt><dd className="mt-1 font-mono">{version}</dd></div>}
          {diagnostics?.gatewayVersion && <div><dt className="text-muted-foreground">服务版本</dt><dd className="mt-1 font-mono">{diagnostics.gatewayVersion}</dd></div>}
        </dl>
      </SettingGroup>
    </div>
  )
}
