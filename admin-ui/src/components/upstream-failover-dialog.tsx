import { useEffect, useState } from 'react'
import { Shuffle } from 'lucide-react'
import { toast } from 'sonner'

import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'
import {
  useUpstreamFailoverConfig, useSetUpstreamFailoverConfig,
} from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

/** 与后端夹取规则一致：429 冷却 1 秒到 24 小时 */
const MIN_COOLDOWN_SECS = 1
const MAX_COOLDOWN_SECS = 86_400

/**
 * 与后端校验对齐：单条检测文案至少 4 个 UTF-8 字节。
 * 过短的子串（如 "err"）极易误伤普通错误文案，后端会直接拒绝，
 * 这里提前拦下来给出更友好的提示（指出具体是哪一条）。
 */
const MIN_PATTERN_BYTES = 4

/** 文本框（一行一条）→ 去空白、去空行的文案列表 */
function parsePatterns(text: string): string[] {
  return text
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line !== '')
}

/** 返回首个过短的文案；全部合法时返回 null */
function findTooShortPattern(patterns: string[]): string | null {
  const encoder = new TextEncoder()
  for (const p of patterns) {
    if (encoder.encode(p).length < MIN_PATTERN_BYTES) return p
  }
  return null
}

/**
 * 上游直通凭据故障转移设置弹窗。
 *
 * 只影响「上游 API」类型凭据（中转站）；Kiro 凭据的封禁/限流检测是另一套，
 * 互不干扰。两个文案列表按行编辑，保存时整份提交（后端 PUT 语义是
 * 「缺省不改」，但编辑界面里所有字段都可见可改，整份提交最不易产生歧义）。
 */
export function UpstreamFailoverDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { data: config, isLoading } = useUpstreamFailoverConfig(open)
  const setConfig = useSetUpstreamFailoverConfig()
  const saving = setConfig.isPending
  const busy = isLoading || saving

  const [enabled, setEnabled] = useState(true)
  const [quotaText, setQuotaText] = useState('')
  const [suspendedText, setSuspendedText] = useState('')
  const [throttleFailover, setThrottleFailover] = useState(true)
  const [cooldownSecs, setCooldownSecs] = useState('60')

  useEffect(() => {
    if (!config) return
    setEnabled(config.enabled)
    setQuotaText(config.quotaPatterns.join('\n'))
    setSuspendedText(config.suspendedPatterns.join('\n'))
    setThrottleFailover(config.throttleFailover)
    setCooldownSecs(String(config.throttleCooldownSecs))
  }, [config])

  const handleSave = async (e: React.FormEvent) => {
    e.preventDefault()

    const quotaPatterns = parsePatterns(quotaText)
    const suspendedPatterns = parsePatterns(suspendedText)
    const shortOne = findTooShortPattern(quotaPatterns) ?? findTooShortPattern(suspendedPatterns)
    if (shortOne !== null) {
      toast.error(`检测文案「${shortOne}」过短（至少 ${MIN_PATTERN_BYTES} 字节），太短的子串容易误伤普通错误`)
      return
    }

    const secs = Number(cooldownSecs)
    if (!Number.isInteger(secs) || secs < MIN_COOLDOWN_SECS || secs > MAX_COOLDOWN_SECS) {
      toast.error(`冷却时长必须是 ${MIN_COOLDOWN_SECS}-${MAX_COOLDOWN_SECS} 之间的整数秒`)
      return
    }

    try {
      await setConfig.mutateAsync({
        enabled,
        quotaPatterns,
        suspendedPatterns,
        throttleFailover,
        throttleCooldownSecs: secs,
      })
      toast.success('已保存，立即生效')
      onOpenChange(false)
    } catch (err) {
      toast.error('保存失败：' + extractErrorMessage(err))
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => { if (!busy) onOpenChange(o) }}>
      <DialogContent className="max-h-[calc(100dvh-3rem)] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Shuffle className="h-4 w-4" />
            上游凭据故障转移
          </DialogTitle>
          <DialogDescription>
            仅作用于「上游 API」类型凭据（中转站）。按响应文案识别欠费/封禁并自动禁用切号，
            429 冷却当前 key 后立即换下一个。修改热生效，无需重启。
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleSave} className="space-y-3 py-1">
          {/* 总开关 */}
          <div className="flex items-center justify-between rounded-md border px-2.5 py-2">
            <div className="space-y-0.5">
              <div className="text-[13px] font-medium">启用故障转移检测</div>
              <div className="text-[11px] text-muted-foreground">
                关闭后回退旧行为：401/403 攒失败计数、429 原地退避重试
              </div>
            </div>
            <Switch checked={enabled} disabled={busy} onCheckedChange={setEnabled} />
          </div>

          {/* 欠费检测文案 */}
          <div className={`space-y-1 ${enabled ? '' : 'opacity-60'}`}>
            <label className="text-[11px] text-muted-foreground">
              欠费 / 额度用尽文案（一行一条，子串匹配、大小写不敏感）
            </label>
            <Textarea
              value={quotaText}
              disabled={busy || !enabled}
              placeholder={'credit balance is too low\ninsufficient_quota\n余额不足'}
              onChange={(e) => setQuotaText(e.target.value)}
              className="min-h-[88px] font-mono text-[12px]"
            />
            <p className="text-[11px] leading-relaxed text-muted-foreground">
              匹配 400/401/402/403 响应体，命中后禁用该凭据（额度用尽）并切号。清空 = 关闭此类检测。
            </p>
          </div>

          {/* 封禁检测文案 */}
          <div className={`space-y-1 ${enabled ? '' : 'opacity-60'}`}>
            <label className="text-[11px] text-muted-foreground">
              封禁 / 停用文案（一行一条，子串匹配、大小写不敏感）
            </label>
            <Textarea
              value={suspendedText}
              disabled={busy || !enabled}
              placeholder={'account suspended\napi key disabled\n已封禁'}
              onChange={(e) => setSuspendedText(e.target.value)}
              className="min-h-[88px] font-mono text-[12px]"
            />
            <p className="text-[11px] leading-relaxed text-muted-foreground">
              匹配 401/403 响应体，命中后立即禁用（封禁态，不参与自愈，需手动恢复）并切号。
            </p>
          </div>

          {/* 429 冷却切号 */}
          <div className={`rounded-md border px-2.5 py-2 ${enabled ? '' : 'opacity-60'}`}>
            <div className="flex items-center justify-between gap-2">
              <div className="space-y-0.5">
                <div className="text-[13px] font-medium">429 冷却切号</div>
                <div className="text-[11px] text-muted-foreground">
                  限流时冷却当前 key 并立即切到下一个，而不是原地退避重试
                </div>
              </div>
              <Switch
                checked={throttleFailover}
                disabled={busy || !enabled}
                onCheckedChange={setThrottleFailover}
              />
            </div>
            <div className="mt-2 flex items-center gap-1.5">
              <span className="shrink-0 text-[11px] text-muted-foreground">
                无 Retry-After 时冷却
              </span>
              <Input
                type="number"
                min={MIN_COOLDOWN_SECS}
                max={MAX_COOLDOWN_SECS}
                step="1"
                value={cooldownSecs}
                disabled={busy || !enabled || !throttleFailover}
                onChange={(e) => setCooldownSecs(e.target.value)}
                className="h-7 w-24 text-xs"
              />
              <span className="text-[11px] text-muted-foreground">秒（上游给了 Retry-After 则优先用它）</span>
            </div>
          </div>

          <DialogFooter className="gap-2">
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={busy}
            >
              取消
            </Button>
            <Button type="submit" disabled={busy}>
              {saving ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
