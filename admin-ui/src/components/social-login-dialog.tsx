import { useState, useEffect, useRef } from 'react'
import { toast } from 'sonner'
import { ExternalLink, CheckCircle, Loader2 } from 'lucide-react'
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
import { startSocialLogin, pollSocialLogin } from '@/api/credentials'
import type { StartSocialLoginResponse } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

interface SocialLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

type Step = 'form' | 'waiting' | 'done'

const POLL_INTERVAL_MS = 2000

export function SocialLoginDialog({ open, onOpenChange, onSuccess }: SocialLoginDialogProps) {
  const [step, setStep] = useState<Step>('form')
  const [priority, setPriority] = useState('0')
  const [email, setEmail] = useState('')
  const [isStarting, setIsStarting] = useState(false)
  const [session, setSession] = useState<StartSocialLoginResponse | null>(null)
  const [credentialId, setCredentialId] = useState<number | null>(null)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    return () => {
      if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
    }
  }, [])

  const handleOpenChange = (v: boolean) => {
    if (!v) {
      if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
      setStep('form')
      setSession(null)
      setCredentialId(null)
      setIsStarting(false)
    }
    onOpenChange(v)
  }

  const handleStart = async () => {
    setIsStarting(true)
    try {
      // 非本机访问时自动启用远程回调模式，无需用户配置
      const isLocal = window.location.hostname === 'localhost' || window.location.hostname === '127.0.0.1'
      const resp = await startSocialLogin({
        priority: parseInt(priority) || 0,
        email: email.trim() || undefined,
        callbackBaseUrl: isLocal ? undefined : window.location.origin,
      })
      setSession(resp)
      setStep('waiting')
      // 立刻打开浏览器
      window.open(resp.portalUrl, '_blank')
      schedulePoll(resp.sessionId)
    } catch (e) {
      toast.error('发起登录失败：' + extractErrorMessage(e))
    } finally {
      setIsStarting(false)
    }
  }

  const schedulePoll = (sessionId: string) => {
    pollTimerRef.current = setTimeout(async () => {
      try {
        const result = await pollSocialLogin(sessionId)
        if (result.status === 'pending') {
          schedulePoll(sessionId)
        } else if (result.status === 'success') {
          setCredentialId(result.credentialId)
          setStep('done')
          onSuccess()
          toast.success(`登录成功，已添加凭据 #${result.credentialId}`)
        } else {
          toast.error('会话已过期，请重新发起登录')
          setStep('form')
          setSession(null)
        }
      } catch (e) {
        toast.error('轮询失败：' + extractErrorMessage(e))
        schedulePoll(sessionId)
      }
    }, POLL_INTERVAL_MS)
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Kiro 账号登录（Google / GitHub）</DialogTitle>
          <DialogDescription>
            通过 Kiro 网页端完成 Social 登录，无需手动导出 refreshToken。
          </DialogDescription>
        </DialogHeader>

        {step === 'form' && (
          <div className="space-y-4 py-2">
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-1.5">
                <label htmlFor="social-priority" className="text-sm font-medium">优先级</label>
                <Input
                  id="social-priority"
                  type="number"
                  min="0"
                  value={priority}
                  onChange={(e) => setPriority(e.target.value)}
                />
              </div>
              <div className="space-y-1.5">
                <label htmlFor="social-email" className="text-sm font-medium">邮箱（可选）</label>
                <Input
                  id="social-email"
                  placeholder="user@example.com"
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                />
              </div>
            </div>
          </div>
        )}

        {step === 'waiting' && session && (
          <div className="space-y-4 py-2">
            <div className="rounded-lg border bg-muted/50 p-4 space-y-3">
              <p className="text-sm text-muted-foreground">
                浏览器应已自动打开 Kiro 登录页，请完成授权。
              </p>
              <a
                href={session.portalUrl}
                target="_blank"
                rel="noopener noreferrer"
                className="inline-flex items-center gap-1.5 text-sm font-medium text-primary hover:underline"
              >
                重新打开登录页
                <ExternalLink className="h-3.5 w-3.5" />
              </a>
            </div>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              正在等待登录完成…
            </div>
          </div>
        )}

        {step === 'done' && (
          <div className="flex flex-col items-center gap-3 py-4">
            <CheckCircle className="h-10 w-10 text-green-500" />
            <p className="text-sm font-medium">登录成功</p>
            <p className="text-xs text-muted-foreground">凭据 #{credentialId} 已添加并启用</p>
          </div>
        )}

        <DialogFooter>
          {step === 'form' && (
            <Button onClick={handleStart} disabled={isStarting}>
              {isStarting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              发起登录
            </Button>
          )}
          {step === 'waiting' && (
            <Button variant="outline" onClick={() => handleOpenChange(false)}>
              取消
            </Button>
          )}
          {step === 'done' && (
            <Button onClick={() => handleOpenChange(false)}>关闭</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
