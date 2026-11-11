// Dashboard dates are Beijing calendar dates, independent of the browser timezone.
const dateFormatter = new Intl.DateTimeFormat('en-US', {
  timeZone: 'Asia/Shanghai', year: 'numeric', month: '2-digit', day: '2-digit',
  hour: '2-digit', hourCycle: 'h23',
})

function parts(date: Date): Record<string, string> {
  return Object.fromEntries(dateFormatter.formatToParts(date).map((p) => [p.type, p.value]))
}

export function statsDateInput(date: Date): string {
  const p = parts(date)
  return `${p.year}-${p.month}-${p.day}`
}

export function formatStatsTimestamp(timestamp: string, granularity: 'hour' | 'day'): string {
  const date = new Date(timestamp)
  const p = parts(date)
  const day = `${p.year}-${p.month}-${p.day}`
  return granularity === 'day' ? day : `${day} ${p.hour}:00`
}

export function presetStatsStartDate(range: '24h' | '7d' | '30d', endDate: string): string {
  const days = range === '24h' ? 1 : range === '7d' ? 6 : 29
  // Arithmetic on the date string must not pass through the browser's local midnight/DST.
  const date = new Date(`${endDate}T00:00:00Z`)
  date.setUTCDate(date.getUTCDate() - days)
  return date.toISOString().slice(0, 10)
}
