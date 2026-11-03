import assert from 'node:assert/strict'
import test from 'node:test'
import { importProxyFields, proxyRequestFields, proxyChoiceOf, sameProxyChoice } from '../src/lib/credential-proxy.ts'

const source = { proxyId: 1, proxyUrl: 'http://old:3128', proxyUsername: 'old-user', proxyPassword: 'old-password' }

test('JSON and KAM imports preserve per-account proxy settings without random assignment', () => {
  assert.deepEqual(importProxyFields({ kind: 'preserve' }, source), source)
  assert.deepEqual(importProxyFields({ kind: 'preserve' }, {}), {
    proxyId: undefined, proxyUrl: undefined, proxyUsername: undefined, proxyPassword: undefined,
  })
})

test('an explicit import proxy replaces the entire file route including stale auth', () => {
  assert.deepEqual(importProxyFields({ kind: 'pool', id: 42 }, source), { proxyId: 42 })
  assert.deepEqual(importProxyFields({ kind: 'direct' }, source), { proxyUrl: 'direct' })
  assert.deepEqual(importProxyFields({ kind: 'global' }, source), { proxyUrl: '' })
})

test('login sends a proxy ID or explicit direct/global choice without exposing passwords', () => {
  assert.deepEqual(proxyRequestFields({ kind: 'pool', id: 42 }), { proxyId: 42 })
  assert.deepEqual(proxyRequestFields({ kind: 'direct' }), { proxyUrl: 'direct' })
  assert.deepEqual(proxyRequestFields({ kind: 'global' }), { proxyUrl: '' })
})

test('relogin retains account proxy unless the user selects a different route', () => {
  const current = proxyChoiceOf({ proxyId: 9, proxyUrl: 'http://old:3128' })
  assert(sameProxyChoice(current, { kind: 'pool', id: 9 }))
  assert(!sameProxyChoice(current, { kind: 'pool', id: 10 }))
  assert.deepEqual(proxyChoiceOf({ proxyUrl: 'direct' }), { kind: 'direct' })
  assert.deepEqual(proxyChoiceOf({}), { kind: 'global' })
})
