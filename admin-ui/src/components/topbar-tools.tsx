import { useEffect, useState } from 'react'
import {
  Activity, RefreshCw, UploadCloud, Settings, Key, Wand2, Eye, EyeOff, Copy,
  Boxes,
  Database,
  MoreHorizontal,
  Rss,
  ShieldAlert,
  ShieldCheck,
} from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent,
  DropdownMenuItem, DropdownMenuLabel,
} from '@/components/ui/dropdown-menu'
import {
  useLoadBalancingMode, useSetLoadBalancingMode,
  useAccountThrottleConfig, useSetAccountThrottleConfig,
  useTokenInflationConfig,
  useAccountRpmLimitConfig, useSetAccountRpmLimitConfig,
  useSelfHealConfig, useSetSelfHealConfig,
} from '@/hooks/use-credentials'
import { useUpdateCheck } from '@/hooks/use-update-check'
import { updateAdminKey, type SelfHealConfigPatch } from '@/api/credentials'
import { extractErrorMessage, generateApiKey } from '@/lib/utils'
import { ImageUpdateDialog } from '@/components/image-update-dialog'
import { CacheEngineDialog } from '@/components/cache-engine-dialog'
import { KeyFeedDialog } from '@/components/key-feed-dialog'
import { AvailableModelsDialog } from '@/components/available-models-dialog'

/**
 * 顶栏右侧通用工具栏：负载均衡切换、可用模型、刷新、在线更新、设置（Key 管理）。
 *
 * 与原 Dashboard 中的工具按钮等价，但全局 Tab 都可访问。刷新按钮会失效
 * 凭据/客户端 Key/统计三类查询，覆盖三个 Tab 的主要数据源。
 */
interface TopbarToolsProps {
  compact?: boolean
}

export function TopbarTools({ compact = false }: TopbarToolsProps) {
  const queryClient = useQueryClient()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()
  const { data: throttleConfig, isLoading: isLoadingThrottle } = useAccountThrottleConfig()
  const { mutate: setThrottleConfig, isPending: isSettingThrottle } = useSetAccountThrottleConfig()
  // 只读：全局倍率的编辑入口已并入「缓存引擎」弹窗的「下发倍率」矩阵
  // （那里的 global 段与 /config/token-inflation 是同一份存储，后端走同一个
  // set_token_inflation_config）。这里保留读取，仅用于齿轮上的「非默认」状态点。
  const { data: inflationConfig } = useTokenInflationConfig()
  const { data: updateCheck } = useUpdateCheck()

  const [imageUpdateOpen, setImageUpdateOpen] = useState(false)
  const [modelsDialogOpen, setModelsDialogOpen] = useState(false)
  const [keyDialogOpen, setKeyDialogOpen] = useState(false)
  const [cacheEngineDialogOpen, setCacheEngineDialogOpen] = useState(false)
  const [keyFeedDialogOpen, setKeyFeedDialogOpen] = useState(false)
  const [newKey, setNewKey] = useState('')
  const [showPlain, setShowPlain] = useState(false)
  const [updating, setUpdating] = useState(false)

  const handleRefresh = () => {
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
    queryClient.invalidateQueries({ queryKey: ['client-keys'] })
    queryClient.invalidateQueries({ queryKey: ['stats'] })
    queryClient.invalidateQueries({ queryKey: ['current-credential-models'] })
    queryClient.invalidateQueries({ queryKey: ['credential-models'] })
    toast.success('已刷新')
  }

  const handleToggleLoadBalancing = () => {
    const cur = loadBalancingData?.mode || 'priority'
    const next = cur === 'priority' ? 'balanced' : 'priority'
    setLoadBalancingMode(next, {
      onSuccess: () => toast.success(`已切换到${next === 'priority' ? '优先级模式' : '均衡负载模式'}`),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const handleToggleFailover = () => {
    const cur = throttleConfig?.failover ?? true
    const next = !cur
    setThrottleConfig({ failover: next }, {
      onSuccess: () => toast.success(next ? '已开启账号级风控故障转移' : '已关闭账号级风控故障转移'),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const openKeyDialog = () => {
    setNewKey('')
    setShowPlain(false)
    setKeyDialogOpen(true)
  }

  const handleUpdateKey = async (e: React.FormEvent) => {
    e.preventDefault()
    const key = newKey.trim()
    if (!key) {
      toast.error('新登录API密钥不能为空')
      return
    }
    setUpdating(true)
    try {
      await updateAdminKey({ newKey: key })
      storage.setApiKey(key)
      toast.success('登录API密钥已更新，已自动切换到新 Key')
      setKeyDialogOpen(false)
      setNewKey('')
    } catch (err) {
      toast.error(`更新失败: ${extractErrorMessage(err)}`)
    } finally {
      setUpdating(false)
    }
  }

  const controls = {
    handleRefresh,
    handleToggleFailover,
    handleToggleLoadBalancing,
    isLoadingMode,
    isLoadingThrottle,
    isSettingMode,
    isSettingThrottle,
    loadBalancingMode: loadBalancingData?.mode,
    openImageUpdate: () => setImageUpdateOpen(true),
    openModels: () => setModelsDialogOpen(true),
    openKeyDialog,
    openCacheEngineDialog: () => setCacheEngineDialogOpen(true),
    openKeyFeedDialog: () => setKeyFeedDialogOpen(true),
    throttleConfig,
    inflationConfig,
    updateCheck,
    updateCooldown: (secs: number) =>
      setThrottleConfig({ cooldownSecs: secs }, {
        onSuccess: () =>
          toast.success(`冷却时长已设为 ${Math.round(secs / 60)} 分钟`),
        onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      }),
  }

  return (
    <>
      {compact ? <CompactTools controls={controls} /> : <FullTools controls={controls} />}
      <ImageUpdateDialog open={imageUpdateOpen} onOpenChange={setImageUpdateOpen} />
      <AvailableModelsDialog
        open={modelsDialogOpen}
        onOpenChange={setModelsDialogOpen}
      />

      <CacheEngineDialog
        open={cacheEngineDialogOpen}
        onOpenChange={setCacheEngineDialogOpen}
      />

      <KeyFeedDialog open={keyFeedDialogOpen} onOpenChange={setKeyFeedDialogOpen} />

      <Dialog
        open={keyDialogOpen}
        onOpenChange={(open) => { if (!updating) setKeyDialogOpen(open) }}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Key className="h-4 w-4" />
              修改登录API密钥
            </DialogTitle>
            <DialogDescription>
              用于登录此管理面板。修改后将自动更新本地存储的 Key，无需重新登录。
            </DialogDescription>
          </DialogHeader>
          <form onSubmit={handleUpdateKey} className="space-y-4 py-2">
            <div className="relative">
              <Input
                type={showPlain ? 'text' : 'password'}
                placeholder="输入或生成新的登录API密钥"
                value={newKey}
                onChange={(e) => setNewKey(e.target.value)}
                disabled={updating}
                autoFocus
                className="pr-20 font-mono text-[13px]"
              />
              <div className="pointer-events-none absolute inset-y-0 right-0 flex items-center pr-1.5">
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={() => setShowPlain((v) => !v)}
                  disabled={updating}
                  title={showPlain ? '隐藏' : '显示'}
                >
                  {showPlain ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={async () => {
                    if (!newKey.trim()) {
                      toast.error('请先输入或生成 Key 再复制')
                      return
                    }
                    try {
                      await navigator.clipboard.writeText(newKey)
                      toast.success('已复制到剪贴板')
                    } catch {
                      toast.error('复制失败，请手动选择文本')
                    }
                  }}
                  disabled={updating}
                  title="复制"
                >
                  <Copy className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
            <div className="flex items-center justify-between gap-2">
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => {
                  const key = generateApiKey('sk-admin-')
                  setNewKey(key)
                  setShowPlain(true)
                }}
                disabled={updating}
              >
                <Wand2 className="h-3.5 w-3.5" />生成随机 Key
              </Button>
              <p className="text-[11px] text-muted-foreground">
                建议生成后立即复制保存，确认更新后即生效。
              </p>
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setKeyDialogOpen(false)} disabled={updating}>
                取消
              </Button>
              <Button type="submit" disabled={updating || !newKey.trim()}>
                {updating ? '更新中…' : '确认更新'}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </>
  )
}

interface ToolControls {
  handleRefresh: () => void
  handleToggleFailover: () => void
  handleToggleLoadBalancing: () => void
  isLoadingMode: boolean
  isLoadingThrottle: boolean
  isSettingMode: boolean
  isSettingThrottle: boolean
  loadBalancingMode?: 'priority' | 'balanced'
  openImageUpdate: () => void
  openKeyDialog: () => void
  openCacheEngineDialog: () => void
  openKeyFeedDialog: () => void
  openModels: () => void
  throttleConfig?: { failover: boolean; cooldownSecs: number }
  /** 只读：编辑入口在缓存引擎弹窗，这里仅用于状态点与菜单后缀 */
  inflationConfig?: { inputMultiplier: number; outputMultiplier: number; cacheMultiplier: number }
  updateCheck?: { hasUpdate: boolean; latestVersion: string; currentVersion: string }
  updateCooldown: (secs: number) => void
}

/**
 * 宽屏工具栏：三个高频动作留成独立图标，其余配置类全部收进「设置」菜单。
 *
 * **为什么不再平铺那 6 个带文字的配置按钮**：它们合计约 900px 且 `shrink-0`，
 * 而 `HeaderActions` 与 `DesktopTabs` 同处一行、共享 `max-w-[1400px]`。
 * 实测（CDP 量 1536/1600/1700/1920/2560 五档）tab 行右边界恒定越过工具区左边界
 * 120px，「请求日志」tab 被盖住且 `elementFromPoint` 命中的不是它自己 —— 即**点不动**。
 * 且该重叠量与视口宽度无关，把窗口拉多宽都不会自愈。
 *
 * 收进菜单后工具区从约 900px 降到约 190px，tab 行不再被挤出。
 * 状态可见性没有丢：菜单项用的就是 `*CompactItems`，展开即见当前值。
 */
function FullTools({ controls }: { controls: ToolControls }) {
  return (
    <>
      <ModelsButton onOpen={controls.openModels} />
      <RefreshButton onRefresh={controls.handleRefresh} />
      <ImageUpdateButton controls={controls} />
      <SettingsMenu controls={controls} />
    </>
  )
}

/**
 * 「设置」齿轮菜单：负载均衡、故障转移、膨胀倍率、缓存引擎、订阅源、自愈、
 * 限流、登录密钥 —— 全部配置项的唯一入口（宽屏用）。
 */
function SettingsMenu({ controls }: { controls: ToolControls }) {
  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" title="设置与参数" className="relative">
          <Settings className="h-4 w-4" />
          {hasNonDefaultState(controls) && <SettingsDot />}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="end"
        className="max-h-[calc(100dvh-4.5rem)] w-72 max-w-[calc(100dvw-1rem)] overflow-x-hidden overflow-y-auto overscroll-contain"
      >
        <SettingsMenuItems controls={controls} />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

/**
 * 配置类菜单项。宽屏的齿轮菜单与窄屏的 ⋯ 菜单共用这一份，
 * 避免两套入口的项目集合漂移（历史上就是这么漂移出不一致的）。
 */
function SettingsMenuItems({ controls }: { controls: ToolControls }) {
  return (
    <>
      <DropdownMenuLabel>调度与限流</DropdownMenuLabel>
      <DropdownMenuItem
        disabled={controls.isLoadingMode || controls.isSettingMode}
        onSelect={controls.handleToggleLoadBalancing}
      >
        <Activity />
        {controls.isLoadingMode
          ? '负载均衡加载中'
          : controls.loadBalancingMode === 'priority'
            ? '当前：优先级 → 切换到均衡负载'
            : '当前：均衡负载 → 切换到优先级'}
      </DropdownMenuItem>
      <ThrottleCompactItems
        config={controls.throttleConfig}
        loading={controls.isLoadingThrottle}
        saving={controls.isSettingThrottle}
        onToggleFailover={controls.handleToggleFailover}
        onChangeCooldown={controls.updateCooldown}
      />
      <SelfHealCompactItems />
      <AccountRpmLimitCompactItems />

      <DropdownMenuLabel>计量与缓存</DropdownMenuLabel>
      {/*
        四套引擎的倍率 + 全局倍率都在这一个弹窗里。原先另有一个「Token 膨胀倍率」入口，
        但它写的就是这里的 global 段（后端同一个 set_token_inflation_config，
        同样的 [1.0, 100.0] 校验）—— 两个入口改同一份数据只会让人以为是两份，故已移除。
      */}
      <DropdownMenuItem onSelect={controls.openCacheEngineDialog}>
        <Database />
        缓存引擎与下发倍率{inflationSuffix(controls)}
      </DropdownMenuItem>

      <DropdownMenuLabel>凭据来源</DropdownMenuLabel>
      <DropdownMenuItem onSelect={controls.openKeyFeedDialog}>
        <Rss />API Key 订阅源
      </DropdownMenuItem>

      <DropdownMenuLabel>密钥管理</DropdownMenuLabel>
      <DropdownMenuItem onSelect={controls.openKeyDialog}>
        <Key />修改登录API密钥（管理面板登录）
      </DropdownMenuItem>
    </>
  )
}

/** 全局倍率在菜单项上的状态后缀（收起后仍能一眼看出是否非默认）。 */
function inflationSuffix(controls: ToolControls) {
  return hasInflation(controls) ? '（倍率生效中）' : ''
}

function hasInflation(controls: ToolControls) {
  const c = controls.inflationConfig
  return !!c && (c.inputMultiplier > 1 || c.outputMultiplier > 1 || c.cacheMultiplier > 1)
}

/**
 * 齿轮上的状态点：存在「非默认」配置时才显示。
 *
 * 平铺按钮收进菜单后，最大的损失是「膨胀中 / 故障转移关」这类非默认状态不再一眼可见。
 * 这个点把该信号补回来 —— 不展开也知道有东西被调过。
 */
function hasNonDefaultState(controls: ToolControls) {
  const throttleOff = controls.throttleConfig
    ? !controls.throttleConfig.failover
    : false
  return hasInflation(controls) || throttleOff
}

function SettingsDot() {
  return (
    <span className="absolute right-1.5 top-1.5 inline-flex h-1.5 w-1.5">
      <span className="inline-flex h-full w-full rounded-full bg-amber-500" />
    </span>
  )
}

/**
 * 窄屏工具栏：连三个高频图标也放不下，全部收进一个 ⋯ 菜单。
 *
 * 配置项直接复用 [`SettingsMenuItems`]，与宽屏齿轮菜单同一份，不会两边漂移。
 * 这里额外多出「刷新 / 可用模型 / 在线更新」三项 —— 宽屏它们是独立图标，窄屏没地方放。
 */
function CompactTools({ controls }: { controls: ToolControls }) {
  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="icon" title="更多操作" className="relative">
          <MoreHorizontal className="h-4 w-4" />
          {(hasNonDefaultState(controls) || controls.updateCheck?.hasUpdate) && <SettingsDot />}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="end"
        className="max-h-[calc(100dvh-4.5rem)] w-72 max-w-[calc(100dvw-1rem)] overflow-x-hidden overflow-y-auto overscroll-contain"
      >
        <DropdownMenuLabel>常用操作</DropdownMenuLabel>
        <DropdownMenuItem onSelect={controls.handleRefresh}>
          <RefreshCw />刷新数据
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.openModels}>
          <Boxes />可用模型
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.openImageUpdate}>
          <UploadCloud />
          镜像在线更新
          {controls.updateCheck?.hasUpdate && (
            <span className="ml-auto text-[10px] text-red-500">有新版</span>
          )}
        </DropdownMenuItem>

        <SettingsMenuItems controls={controls} />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}


function ModelsButton({ onOpen }: { onOpen: () => void }) {
  return (
    <Button variant="ghost" size="icon" onClick={onOpen} title="可用模型">
      <Boxes className="h-4 w-4" />
    </Button>
  )
}

function RefreshButton({ onRefresh }: { onRefresh: () => void }) {
  return (
    <Button variant="ghost" size="icon" onClick={onRefresh} title="刷新">
      <RefreshCw className="h-4 w-4" />
    </Button>
  )
}

function ImageUpdateButton({ controls }: { controls: ToolControls }) {
  return (
    <Button
      variant="ghost"
      size="icon"
      onClick={controls.openImageUpdate}
      title={imageUpdateTitle(controls.updateCheck)}
      className="relative"
    >
      <UploadCloud className="h-4 w-4" />
      {controls.updateCheck?.hasUpdate && <UpdateDot />}
    </Button>
  )
}

function imageUpdateTitle(updateCheck: ToolControls['updateCheck']) {
  if (!updateCheck?.hasUpdate) return '镜像在线更新'
  return `发现新版本 v${updateCheck.latestVersion}（当前 v${updateCheck.currentVersion}）`
}

function UpdateDot() {
  return (
    <span className="absolute right-1 top-1 inline-flex h-2 w-2 items-center justify-center">
      <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-red-400 opacity-75" />
      <span className="relative inline-flex h-2 w-2 rounded-full bg-red-500" />
    </span>
  )
}

interface ThrottleConfigButtonProps {
  config?: { failover: boolean; cooldownSecs: number }
  loading: boolean
  saving: boolean
  onToggleFailover: () => void
  onChangeCooldown: (secs: number) => void
}

interface ThrottleState {
  cooldownMin: number
  cooldownSecs: number
  failover: boolean
}

interface CustomCooldownFormProps {
  cooldownMin: number
  customMin: string
  disabled: boolean
  onCustomMinChange: (value: string) => void
  onSubmit: (e: React.FormEvent) => void
}

const COOLDOWN_PRESETS = [
  { label: '5 分钟', secs: 5 * 60 },
  { label: '15 分钟', secs: 15 * 60 },
  { label: '30 分钟', secs: 30 * 60 },
  { label: '1 小时', secs: 60 * 60 },
  { label: '2 小时', secs: 2 * 60 * 60 },
]

const DEFAULT_COOLDOWN_SECS = 30 * 60
const SECONDS_PER_MINUTE = 60
const MIN_CUSTOM_COOLDOWN_MINUTES = 1
const MAX_CUSTOM_COOLDOWN_MINUTES = 1440

/**
 * 故障转移开关 + 冷却时长设置（紧凑下拉）
 *
 * 主按钮文案显示当前状态；下拉里:
 * - 顶部一个 Switch 切换 failover
 * - 5 个预设时长 + 一个自定义输入（分钟）
 */
function ThrottleCooldownPanel({
  customMin, saving, state, onChangeCooldown, onCustomMinChange, onDone, onSubmitCustom,
}: {
  customMin: string
  saving: boolean
  state: ThrottleState
  onChangeCooldown: (secs: number) => void
  onCustomMinChange: (value: string) => void
  onDone?: () => void
  onSubmitCustom: (e: React.FormEvent) => void
}) {
  const disabled = saving || !state.failover

  return (
    <>
      <DropdownMenuLabel className="pt-1">冷却时长</DropdownMenuLabel>
      <div className={cooldownPanelClassName(state.failover)}>
        <CooldownPresetButtons
          cooldownSecs={state.cooldownSecs}
          disabled={disabled}
          onChangeCooldown={onChangeCooldown}
          onDone={onDone}
        />
        <CustomCooldownForm
          cooldownMin={state.cooldownMin}
          customMin={customMin}
          disabled={disabled}
          onCustomMinChange={onCustomMinChange}
          onSubmit={onSubmitCustom}
        />
      </div>
    </>
  )
}

function CustomCooldownForm({
  cooldownMin, customMin, disabled, onCustomMinChange, onSubmit,
}: CustomCooldownFormProps) {
  return (
    <form onSubmit={onSubmit} className="mt-2 flex items-center gap-1.5">
      <Input
        type="number"
        min={MIN_CUSTOM_COOLDOWN_MINUTES}
        max={MAX_CUSTOM_COOLDOWN_MINUTES}
        placeholder={`自定义（当前 ${cooldownMin}）`}
        value={customMin}
        onChange={(e) => onCustomMinChange(e.target.value)}
        disabled={disabled}
        className="h-7 text-xs"
      />
      <span className="text-xs text-muted-foreground">分钟</span>
      <Button
        type="submit"
        size="sm"
        variant="outline"
        className="h-7 text-xs"
        disabled={disabled || !customMin.trim()}
      >
        保存
      </Button>
    </form>
  )
}

function ThrottleCompactItems(props: ThrottleConfigButtonProps) {
  const { loading, saving, onToggleFailover, onChangeCooldown } = props
  const [customMin, setCustomMin] = useState('')
  const state = readThrottleState(props.config)
  const busy = loading || saving

  const submitCustom = (e: React.FormEvent) => {
    e.preventDefault()
    const min = parseInt(customMin, 10)
    if (invalidCooldownMinutes(min)) {
      toast.error('请输入 1-1440 之间的分钟数')
      return
    }
    onChangeCooldown(min * SECONDS_PER_MINUTE)
    setCustomMin('')
  }

  return (
    <>
      <DropdownMenuLabel>故障转移</DropdownMenuLabel>
      <DropdownMenuItem
        disabled={busy}
        onSelect={onToggleFailover}
      >
        {state.failover ? <ShieldCheck /> : <ShieldAlert />}
        {compactThrottleText(loading, state)}
      </DropdownMenuItem>
      <ThrottleCooldownPanel
        customMin={customMin}
        saving={busy}
        state={state}
        onChangeCooldown={onChangeCooldown}
        onCustomMinChange={setCustomMin}
        onSubmitCustom={submitCustom}
      />
    </>
  )
}

// ============ 自愈治理 ============

const SELF_HEAL_INTERVAL_PRESETS = [
  { label: '不冷却', secs: 0 },
  { label: '1 分钟', secs: 60 },
  { label: '5 分钟', secs: 5 * 60 },
  { label: '15 分钟', secs: 15 * 60 },
  { label: '30 分钟', secs: 30 * 60 },
]

/**
 * 自愈治理设置（下拉）：
 * - 开关：是否启用凭据自愈
 * - 冷却间隔：两次自愈的最小间隔（打断持续 403 死循环的关键）
 * - 连续上限：连续自愈达到该轮数且期间无成功则停止（0=不限）
 * - 只读观测：凭据最大连续轮数 / 累计恢复凭据次数
 */
function useSelfHealPanelState(resetInput: boolean) {
  const { data: config, isLoading } = useSelfHealConfig()
  const { mutate, isPending } = useSetSelfHealConfig()
  const [roundsInput, setRoundsInput] = useState('')

  useEffect(() => {
    if (resetInput) setRoundsInput('')
  }, [resetInput])

  const enabled = config?.enabled ?? true
  const busy = isLoading || isPending

  const save = (patch: SelfHealConfigPatch, msg: string) => {
    mutate(patch, {
      onSuccess: () => toast.success(msg),
      onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
    })
  }

  const submitRounds = (e: React.FormEvent) => {
    e.preventDefault()
    const n = parseInt(roundsInput, 10)
    if (Number.isNaN(n) || n < 0 || n > 1000) {
      toast.error('请输入 0-1000 之间的轮数（0=不限）')
      return
    }
    save({ maxConsecutiveRounds: n }, n === 0 ? '连续自愈已设为不限' : `连续自愈上限已设为 ${n} 轮`)
    setRoundsInput('')
  }

  return {
    busy,
    config,
    enabled,
    isLoading,
    roundsInput,
    save,
    setRoundsInput,
    submitRounds,
  }
}

type SelfHealPanelState = ReturnType<typeof useSelfHealPanelState>

function SelfHealConfigPanel({
  busy,
  config,
  enabled,
  roundsInput,
  save,
  setRoundsInput,
  submitRounds,
}: SelfHealPanelState) {
  return (
    <>
      <DropdownMenuLabel>凭据自愈</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <div className="min-w-0 text-xs">
            <div className="font-medium">{enabled ? '已启用' : '已关闭'}</div>
            <div className="text-muted-foreground">
              当前请求池全灭时按作用域恢复凭据
            </div>
          </div>
          <Switch
            checked={enabled}
            disabled={busy}
            onCheckedChange={(v) => save({ enabled: v }, v ? '已开启凭据自愈' : '已关闭凭据自愈')}
          />
        </div>
        {config && (
          <div className="mt-2 flex items-center justify-between rounded-md bg-secondary/20 px-2.5 py-1.5 text-xs text-muted-foreground">
            <span>连续 {config.consecutiveRounds} 轮</span>
            <span>累计恢复 {config.totalCount} 次</span>
          </div>
        )}
      </div>

      <DropdownMenuLabel className="pt-1">403 封禁识别</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <div className="min-w-0 text-xs">
            <div className="font-medium">
              {config?.suspendedDetectionEnabled ?? true ? '已启用' : '已关闭'}
            </div>
            <div className="text-muted-foreground">
              命中封禁文案的 403 立即禁用，不参与自愈
            </div>
          </div>
          <Switch
            checked={config?.suspendedDetectionEnabled ?? true}
            disabled={busy}
            onCheckedChange={(v) =>
              save({ suspendedDetectionEnabled: v }, v ? '已开启 403 封禁识别' : '已关闭 403 封禁识别')
            }
          />
        </div>
      </div>

      <DropdownMenuLabel className="pt-1">自愈冷却间隔</DropdownMenuLabel>
      <div className={cooldownPanelClassName(enabled)}>
        <div className="grid grid-cols-3 gap-1.5">
          {SELF_HEAL_INTERVAL_PRESETS.map((p) => (
            <Button
              key={p.secs}
              size="sm"
              variant={config?.minIntervalSecs === p.secs ? 'default' : 'outline'}
              className="h-7 text-xs"
              disabled={busy || !enabled}
              onClick={() => save({ minIntervalSecs: p.secs }, `自愈冷却已设为「${p.label}」`)}
            >
              {p.label}
            </Button>
          ))}
        </div>

        <DropdownMenuLabel className="px-0 pt-2">连续自愈上限（0=不限）</DropdownMenuLabel>
        <form onSubmit={submitRounds} className="mt-1 flex items-center gap-1.5">
          <Input
            type="number"
            min={0}
            max={1000}
            placeholder={`当前 ${config?.maxConsecutiveRounds ?? 5} 轮`}
            value={roundsInput}
            onChange={(e) => setRoundsInput(e.target.value)}
            disabled={busy || !enabled}
            className="h-7 min-w-0 text-xs"
          />
          <span className="text-xs text-muted-foreground">轮</span>
          <Button
            type="submit"
            size="sm"
            variant="outline"
            className="h-7 text-xs"
            disabled={busy || !enabled || !roundsInput.trim()}
          >
            保存
          </Button>
        </form>
      </div>
    </>
  )
}

/** 紧凑菜单复用完整配置，避免移动端丢失治理选项。 */
function SelfHealCompactItems() {
  const panel = useSelfHealPanelState(false)
  return <SelfHealConfigPanel {...panel} />
}

const RPM_LIMIT_PRESETS = [10, 30, 60, 120, 300]
const MIN_RPM_LIMIT = 1
const MAX_RPM_LIMIT = 100000

/**
 * 单账号 RPM 主动限流：开关 + 每分钟上限设置（紧凑下拉）。
 *
 * 开启后每个账号独立维护 60 秒滑动窗口，达到上限时该账号被临时排除出候选，
 * 请求自动故障转移到下一个可用账号；全部超限时返回 429。
 */
function useAccountRpmLimitPanelState(resetInput: boolean) {
  const { data: config, isLoading } = useAccountRpmLimitConfig()
  const { mutate, isPending } = useSetAccountRpmLimitConfig()
  const [limitInput, setLimitInput] = useState('')

  useEffect(() => {
    if (resetInput) setLimitInput('')
  }, [resetInput])

  const enabled = config?.enabled ?? false
  const limit = config?.limit ?? 60
  const busy = isLoading || isPending

  const save = (patch: { enabled?: boolean; limit?: number }, msg: string) => {
    mutate(patch, {
      onSuccess: () => toast.success(msg),
      onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
    })
  }

  const submitLimit = (e: React.FormEvent) => {
    e.preventDefault()
    const n = parseInt(limitInput, 10)
    if (Number.isNaN(n) || n < MIN_RPM_LIMIT || n > MAX_RPM_LIMIT) {
      toast.error(`请输入 ${MIN_RPM_LIMIT}-${MAX_RPM_LIMIT} 之间的次数`)
      return
    }
    save({ limit: n }, `单账号 RPM 上限已设为 ${n} 次/分钟`)
    setLimitInput('')
  }

  return {
    busy,
    config,
    enabled,
    isLoading,
    limit,
    limitInput,
    save,
    setLimitInput,
    submitLimit,
  }
}

type AccountRpmLimitPanelState = ReturnType<typeof useAccountRpmLimitPanelState>

function AccountRpmLimitPanel({
  busy,
  enabled,
  limit,
  limitInput,
  save,
  setLimitInput,
  submitLimit,
}: AccountRpmLimitPanelState) {
  return (
    <>
      <DropdownMenuLabel>单账号每分钟请求限流</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <div className="min-w-0 text-xs">
            <div className="font-medium">{enabled ? '已启用' : '已关闭'}</div>
            <div className="text-muted-foreground leading-snug">
              单账号超过每分钟上限时临时跳过并切换到下一个可用账号
            </div>
          </div>
          <Switch
            checked={enabled}
            disabled={busy}
            onCheckedChange={(v) => save({ enabled: v }, v ? '已开启单账号限流' : '已关闭单账号限流')}
          />
        </div>
      </div>

      <DropdownMenuLabel className="pt-1">每分钟上限</DropdownMenuLabel>
      <div className={cooldownPanelClassName(enabled)}>
        <div className="grid grid-cols-3 gap-1.5">
          {RPM_LIMIT_PRESETS.map((n) => (
            <Button
              key={n}
              size="sm"
              variant={limit === n ? 'default' : 'outline'}
              className="h-7 text-xs"
              disabled={busy || !enabled}
              onClick={() => save({ limit: n }, `单账号 RPM 上限已设为 ${n} 次/分钟`)}
            >
              {n}
            </Button>
          ))}
        </div>

        <DropdownMenuLabel className="px-0 pt-2">自定义（次/分钟）</DropdownMenuLabel>
        <form onSubmit={submitLimit} className="mt-1 flex items-center gap-1.5">
          <Input
            type="number"
            min={MIN_RPM_LIMIT}
            max={MAX_RPM_LIMIT}
            placeholder={`当前 ${limit} 次`}
            value={limitInput}
            onChange={(e) => setLimitInput(e.target.value)}
            disabled={busy || !enabled}
            className="h-7 min-w-0 text-xs"
          />
          <span className="text-xs text-muted-foreground">次</span>
          <Button
            type="submit"
            size="sm"
            variant="outline"
            className="h-7 text-xs"
            disabled={busy || !enabled || !limitInput.trim()}
          >
            保存
          </Button>
        </form>
      </div>
    </>
  )
}

/** 紧凑菜单复用完整配置，避免移动端只能切换开关。 */
function AccountRpmLimitCompactItems() {
  const panel = useAccountRpmLimitPanelState(false)
  return <AccountRpmLimitPanel {...panel} />
}

function CooldownPresetButtons({
  cooldownSecs, disabled, onChangeCooldown, onDone,
}: {
  cooldownSecs: number
  disabled: boolean
  onChangeCooldown: (secs: number) => void
  onDone?: () => void
}) {
  return (
    <div className="grid grid-cols-3 gap-1">
      {COOLDOWN_PRESETS.map((preset) => (
        <CooldownPresetButton
          key={preset.secs}
          active={preset.secs === cooldownSecs}
          disabled={disabled}
          label={preset.label}
          secs={preset.secs}
          onChangeCooldown={onChangeCooldown}
          onDone={onDone}
        />
      ))}
    </div>
  )
}

function CooldownPresetButton({
  active, disabled, label, secs, onChangeCooldown, onDone,
}: {
  active: boolean
  disabled: boolean
  label: string
  secs: number
  onChangeCooldown: (secs: number) => void
  onDone?: () => void
}) {
  return (
    <Button
      type="button"
      size="sm"
      variant={active ? 'default' : 'outline'}
      className="h-7 text-xs"
      disabled={disabled}
      onClick={() => {
        if (!active) onChangeCooldown(secs)
        onDone?.()
      }}
    >
      {label}
    </Button>
  )
}

function secondsToMinutes(seconds: number) {
  return Math.round(seconds / SECONDS_PER_MINUTE)
}

function readThrottleState(
  config: ThrottleConfigButtonProps['config'],
): ThrottleState {
  const cooldownSecs = config?.cooldownSecs ?? DEFAULT_COOLDOWN_SECS
  return {
    cooldownMin: secondsToMinutes(cooldownSecs),
    cooldownSecs,
    failover: config?.failover ?? true,
  }
}

function compactThrottleText(loading: boolean, state: ThrottleState) {
  if (loading) return '故障转移加载中'
  if (!state.failover) return '开启故障转移'
  return `关闭故障转移 · ${state.cooldownMin}m`
}

function invalidCooldownMinutes(minutes: number) {
  return (
    Number.isNaN(minutes) ||
    minutes < MIN_CUSTOM_COOLDOWN_MINUTES ||
    minutes > MAX_CUSTOM_COOLDOWN_MINUTES
  )
}

function cooldownPanelClassName(failover: boolean) {
  return `px-2 pb-2 ${failover ? '' : 'opacity-60'}`
}
