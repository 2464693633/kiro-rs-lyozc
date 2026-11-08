import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { getInputTokenDeduction, setInputTokenDeduction } from '@/api/credentials'
import { SettingGroup, SettingNumber } from '@/components/console/setting-row'
import { Button } from '@/components/ui/button'
import { extractErrorMessage } from '@/lib/utils'

const QUERY_KEY = ['inputTokenDeduction']

export function InputTokenDeductionSetting() {
  const queryClient = useQueryClient()
  const query = useQuery({ queryKey: QUERY_KEY, queryFn: getInputTokenDeduction, retry: 1 })
  const mutation = useMutation({
    mutationFn: setInputTokenDeduction,
    onSuccess: async (data) => {
      await queryClient.cancelQueries({ queryKey: QUERY_KEY })
      queryClient.setQueryData(QUERY_KEY, data)
    },
  })

  return (
    <SettingGroup
      title="输入 Token 扣减"
      description="对每次上游模型调用，先从输入总量扣除指定 Token，再拆分缓存用量；API 返回和后台新增记录使用相同结果。输出 Token 与实际积分不变。"
    >
      <SettingNumber
        label="每次调用扣减"
        hint="0 表示关闭；可设置为 2000，用于承担上游基础上下文开销。输入不足时归零。搜索等多轮调用逐轮扣减，历史记录保持原值。保存后即时生效，无需重启。"
        value={query.data?.inputTokenDeduction ?? 0}
        onCommit={(value) => mutation.mutate(value)}
        min={0}
        max={1_000_000}
        unit="Token"
        presets={[0, 1900, 2000]}
        disabled={!query.data || query.isError || mutation.isPending}
        pending={mutation.isPending}
        saved={mutation.isSuccess}
      />
      {query.isError && (
        <div className="flex items-center gap-2 text-sm text-destructive" role="alert">
          <span>读取失败：{extractErrorMessage(query.error)}</span>
          <Button type="button" size="sm" variant="outline" disabled={query.isFetching} onClick={() => void query.refetch()}>重试</Button>
        </div>
      )}
      {mutation.isError && (
        <p className="text-sm text-destructive" role="alert">保存失败：{extractErrorMessage(mutation.error)}。请重试；连接中断时请刷新确认当前值。</p>
      )}
    </SettingGroup>
  )
}
