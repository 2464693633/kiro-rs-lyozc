import assert from 'node:assert/strict'
import test from 'node:test'
import { readFile } from 'node:fs/promises'
import http from 'node:http'
import vm from 'node:vm'
import ts from 'typescript'
import axios from 'axios'

test('slow IdC startup survives 15 seconds and login routes preserve their proxy payload', { timeout: 25000 }, async () => {
  const received = []
  const configs = []
  const server = http.createServer(async (req, res) => {
    const parts = []
    for await (const part of req) parts.push(part)
    received.push({ url: req.url, body: parts.length ? JSON.parse(Buffer.concat(parts).toString()) : undefined })
    if (req.url === '/auth/idc/start') await new Promise((resolve) => setTimeout(resolve, 16000))
    res.setHeader('Content-Type', 'application/json')
    res.end(JSON.stringify({ sessionId: 'fixture-session', status: 'pending' }))
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  const baseURL = `http://127.0.0.1:${server.address().port}`
  try {
    // Execute the real API wrappers with real Axios HTTP timing. Only storage
    // and the base URL are replaced; no production credentials are accessed.
    const source = await readFile(new URL('../src/api/credentials.ts', import.meta.url), 'utf8')
    const compiled = ts.transpileModule(source, {
      compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022, esModuleInterop: true },
    }).outputText
    const exports = {}
    vm.runInNewContext(compiled, {
      exports,
      require: (name) => {
        if (name === '@/lib/storage') return { storage: { getApiKey: () => 'fixture-only' } }
        if (name === 'axios') return { create: (options) => {
          const client = axios.create({ ...options, baseURL, proxy: false })
          client.interceptors.request.use((config) => { configs.push({ url: config.url, timeout: config.timeout }); return config })
          return client
        } }
        throw new Error(`Unexpected dependency: ${name}`)
      },
    })
    const payload = { region: 'us-east-1', proxyId: 42, groups: ['fixture-group'] }
    const response = await exports.startIdcLogin(payload)
    assert.equal(response.sessionId, 'fixture-session')
    assert.deepEqual(received[0].body, payload)
    await exports.startIdcRelogin(7, payload)
    await exports.pollIdcLogin('fixture-session')
    await exports.pollIdcRelogin(7, 'fixture-session')
    await exports.pollSocialLogin('fixture-session')
    await exports.completeSocialLogin('fixture-session', { code: 'fixture-code' })
    await exports.pollSocialRelogin(7, 'fixture-session')
    await exports.completeSocialRelogin(7, 'fixture-session', { code: 'fixture-code' })
    await exports.getCredentials()
    assert.deepEqual(received[1].body, payload)
    assert(configs.filter((c) => c.url.endsWith('/idc/start')).every((c) => c.timeout > 60000 && c.timeout <= 120000))
    assert(configs.filter((c) => c.url.includes('/poll/') || c.url.includes('/complete/')).every((c) => c.timeout > 60000 && c.timeout <= 180000))
    assert.equal(configs.find((c) => c.url === '/credentials').timeout, 15000)
  } finally {
    server.closeAllConnections()
    await new Promise((resolve) => server.close(resolve))
  }
})
