import { GroupMultiSelect } from '@/components/group-select'
import { useGroupOptions } from '@/hooks/use-groups'

/** 所有账号创建入口共用，登录前选择的分组随凭据一起保存。 */
export function AccountGroupField({
  value,
  onChange,
  disabled,
  importing = false,
}: {
  value: string[]
  onChange: (groups: string[]) => void
  disabled?: boolean
  importing?: boolean
}) {
  const options = useGroupOptions()
  return (
    <div className="space-y-2" role="group" aria-label="账号分组">
      <p className="text-sm font-medium">账号分组（可多选）</p>
      <GroupMultiSelect value={value} options={options} onChange={onChange} disabled={disabled} />
      <p className="text-xs text-muted-foreground">
        {importing
          ? '选择后统一使用所选分组；不选则保留文件中的分组，文件未指定时不分组。'
          : '可选。账号创建后直接归入所选分组；不选则不分组。'}
      </p>
    </div>
  )
}
