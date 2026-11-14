import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { getReportedInputTokens, setReportedInputTokens } from '@/api/credentials'
import { SettingGroup, SettingNumber } from '@/components/console/setting-row'
import { Button } from '@/components/ui/button'
import { extractErrorMessage } from '@/lib/utils'

const QUERY_KEY = ['reportedInputTokens']

export function ReportedInputTokensSetting() {
  const queryClient = useQueryClient()
  const query = useQuery({ queryKey: QUERY_KEY, queryFn: getReportedInputTokens, retry: 1 })
  const mutation = useMutation({
    mutationFn: setReportedInputTokens,
    onSuccess: async (data) => {
      await queryClient.cancelQueries({ queryKey: QUERY_KEY })
      queryClient.setQueryData(QUERY_KEY, data)
    },
  })

  return (
    <SettingGroup
      title="固定上报输入 Token"
      description="把返回给客户端的 input_tokens 固定成指定数量，命中的缓存读取照常上报，未被缓存覆盖的输入计入 cache_creation_input_tokens。只改上报口径：缓存条目、输出 Token 和上游实际积分都不变。"
    >
      <SettingNumber
        label="固定上报"
        hint="0 表示关闭，按真实拆分上报。设为 2 时每次请求的 input_tokens 恒为 2，其余输入并入缓存创建量，三项之和仍等于真实输入总量。输入总量不足该值时按总量上报。关闭了缓存计量模拟的客户端 Key 不受影响。保存后即时生效，无需重启。"
        value={query.data?.reportedInputTokens ?? 0}
        onCommit={(value) => mutation.mutate(value)}
        min={0}
        max={1_000}
        unit="Token"
        presets={[0, 2, 10]}
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
