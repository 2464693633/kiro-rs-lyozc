import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { Tags, Settings2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { useUpdateCredential } from '@/hooks/use-credentials'
import { useGroupOptions } from '@/hooks/use-groups'
import { extractErrorMessage, cn } from '@/lib/utils'
import { GroupMultiSelect } from '@/components/group-select'
import { CredentialMetadataEditor, metadataDefaults } from '@/components/credential-metadata-field'
import {
  ProxySelector,
  proxyChoiceOf,
  proxyRequestFields,
  sameProxyChoice,
  type ProxyChoice,
} from '@/components/proxy-selector'
import type { CredentialMetadata, CredentialMetadataSchema, CredentialStatusItem } from '@/types/api'

type TabKey = 'general' | 'metadata'

const TABS: { key: TabKey; label: string; icon: React.ReactNode }[] = [
  { key: 'general', label: '基本', icon: <Settings2 className="h-3.5 w-3.5" /> },
  { key: 'metadata', label: 'Metadata', icon: <Tags className="h-3.5 w-3.5" /> },
]

interface EditCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credential: CredentialStatusItem
  metadataSchema?: CredentialMetadataSchema
}

/** 状态接口的 metadata 带展示信息；编辑时只回填其中的实际值。 */
function metadataValues(metadata: CredentialStatusItem['metadata']): Partial<CredentialMetadata> {
  return Object.fromEntries(
    Object.entries(metadata ?? {}).map(([key, detail]) => [key, detail.value]),
  ) as Partial<CredentialMetadata>
}

export function EditCredentialDialog({
  open,
  onOpenChange,
  credential,
  metadataSchema,
}: EditCredentialDialogProps) {
  const [activeTab, setActiveTab] = useState<TabKey>('general')
  const [email, setEmail] = useState(credential.email ?? '')
  const [proxyChoice, setProxyChoice] = useState<ProxyChoice>(() => proxyChoiceOf(credential))
  const [groups, setGroups] = useState<string[]>(credential.groups ?? [])
  const [sourceChannel, setSourceChannel] = useState(credential.sourceChannel ?? '')
  const [metadata, setMetadata] = useState<CredentialMetadata>(
    { ...metadataDefaults(metadataSchema), ...metadataValues(credential.metadata) },
  )
  const groupOptions = useGroupOptions()

  // 每次打开时重置表单为当前凭据值
  useEffect(() => {
    if (open) {
      setActiveTab('general')
      setEmail(credential.email ?? '')
      setProxyChoice(proxyChoiceOf(credential))
      setGroups(credential.groups ?? [])
      setSourceChannel(credential.sourceChannel ?? '')
      setMetadata({
        ...metadataDefaults(metadataSchema),
        ...metadataValues(credential.metadata),
      })
    }
  }, [open, credential, metadataSchema])

  const { mutate, isPending } = useUpdateCredential()

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()

    mutate(
      {
        id: credential.id,
        req: {
          email: email,
          // 只在改了代理选择时提交代理字段
          ...(sameProxyChoice(proxyChoiceOf(credential), proxyChoice) ? {} : proxyRequestFields(proxyChoice)),
          groups: groups,
          sourceChannel: sourceChannel,
          metadata,
        },
      },
      {
        onSuccess: (data) => {
          toast.success(data.message)
          onOpenChange(false)
        },
        onError: (error: unknown) => {
          toast.error(`更新失败: ${extractErrorMessage(error)}`)
        },
      }
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            编辑凭据 #{credential.id}
          </DialogTitle>
          <DialogDescription>
            修改凭据标识、分组、Metadata 与代理配置。
          </DialogDescription>
        </DialogHeader>

        {/* 标签导航 */}
        <nav className="-mx-1 flex gap-0.5" aria-label="编辑分区">
          {TABS.map((tab) => (
            <button
              key={tab.key}
              type="button"
              onClick={() => setActiveTab(tab.key)}
              className={cn(
                'inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-[13px] transition-colors',
                'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40',
                activeTab === tab.key
                  ? 'bg-primary/12 font-medium text-foreground'
                  : 'text-muted-foreground hover:bg-accent/60 hover:text-foreground',
              )}
            >
              {tab.icon}
              {tab.label}
            </button>
          ))}
        </nav>

        <form onSubmit={handleSubmit}>
          <div className="min-h-[280px] space-y-4 py-2">
            {activeTab === 'general' && (
              <>
                {/* 邮箱 */}
                <div className="space-y-2">
                  <label htmlFor="email" className="text-sm font-medium">
                    邮箱（用于显示标识）
                  </label>
                  <Input
                    id="email"
                    type="email"
                    placeholder="例: user@example.com"
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    留空则显示凭据 ID，清除请提交空值
                  </p>
                </div>

                {/* 账号分组 */}
                <div className="space-y-2">
                  <label className="text-sm font-medium">账号分组</label>
                  <GroupMultiSelect
                    value={groups}
                    options={groupOptions}
                    onChange={setGroups}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    绑定了某分组的客户端 Key 只会调度到含该分组的账号。不选表示不属于任何分组。
                  </p>
                </div>

                {/* 账号来源渠道 */}
                <div className="space-y-2">
                  <label htmlFor="sourceChannel" className="text-sm font-medium">
                    账号来源渠道（备注）
                  </label>
                  <Input
                    id="sourceChannel"
                    placeholder="例: 官方, 转售商A, 采购平台X"
                    value={sourceChannel}
                    onChange={(e) => setSourceChannel(e.target.value)}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    纯备注，标记此账号的购买来源/渠道，便于追踪。留空表示清除。
                  </p>
                </div>

                {/* 代理 */}
                <div className="space-y-2">
                  <label className="text-sm font-medium">代理</label>
                  <ProxySelector value={proxyChoice} onChange={setProxyChoice} disabled={isPending} />
                  <p className="text-xs text-muted-foreground">
                    从 IP 管理中选择，或使用全局代理、直连。新代理请先到 IP 管理添加。
                  </p>
                </div>
              </>
            )}

            {activeTab === 'metadata' && (
              <CredentialMetadataEditor
                schema={metadataSchema}
                value={metadata}
                onChange={setMetadata}
                disabled={isPending}
              />
            )}
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={isPending}
            >
              取消
            </Button>
            <Button type="submit" disabled={isPending}>
              {isPending ? '保存中...' : '保存'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
