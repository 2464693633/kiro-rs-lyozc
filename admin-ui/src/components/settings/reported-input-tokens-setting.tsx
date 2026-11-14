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
      description="从原输入总量预留指定数量作为 input_tokens，其余输入分为缓存创建和读取。未设置读取目标时保留自然命中量；设置目标后按缓存总量分配。上游精确用量和本地估算使用相同规则，输出 Token 和实际积分不变。"
    >
      <SettingNumber
        label="固定上报"
        hint="0 表示关闭，按真实拆分上报。设为 2 时每个客户端请求预留 2 Token，内部多轮调用合并后只预留一次；新增输入计入缓存创建，命中部分计入读取，三项之和仍等于原输入总量。输入总量不足该值时按总量上报。关闭了缓存计量模拟的客户端 Key 不受影响。保存后即时生效，无需重启。"
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
