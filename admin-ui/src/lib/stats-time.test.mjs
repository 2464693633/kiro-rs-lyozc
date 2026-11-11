import test from 'node:test'
import assert from 'node:assert/strict'
import { statsDateInput, formatStatsTimestamp, presetStatsStartDate } from './stats-time.ts'

test('UTC timestamps display Beijing midnight rather than 08:00 or 24:00', () => {
  assert.equal(formatStatsTimestamp('2026-11-10T16:00:00Z', 'hour'), '2026-11-11 00:00')
  assert.equal(formatStatsTimestamp('2026-11-10T23:00:00Z', 'hour'), '2026-11-11 07:00')
  assert.equal(formatStatsTimestamp('2026-11-11T16:00:00Z', 'day'), '2026-11-12')
})

test('date picker uses the Beijing calendar across UTC midnight', () => {
  assert.equal(statsDateInput(new Date('2026-11-11T15:59:59Z')), '2026-11-11')
  assert.equal(statsDateInput(new Date('2026-11-11T16:00:00Z')), '2026-11-12')
})

test('preset calendar arithmetic handles month and leap-year boundaries', () => {
  assert.equal(presetStatsStartDate('24h', '2026-11-12'), '2026-11-11')
  assert.equal(presetStatsStartDate('7d', '2026-11-03'), '2026-10-28')
  assert.equal(presetStatsStartDate('30d', '2024-03-01'), '2024-02-01')
})
