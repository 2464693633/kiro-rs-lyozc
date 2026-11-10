// Run after building admin-ui. All API calls are mocked locally; no accounts are modified.
const assert = require('node:assert/strict')
const fs = require('node:fs/promises')
const path = require('node:path')
const http = require('node:http')
const { chromium } = require('playwright')

async function main() {
  const dist = path.resolve(__dirname, '../dist')
  const server = http.createServer(async (req, res) => {
    try {
      const relative = new URL(req.url, 'http://localhost').pathname.replace(/^\/admin\//, '')
      const file = path.resolve(dist, relative || 'index.html')
      if (!file.startsWith(dist + path.sep)) return res.writeHead(404).end()
      const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' }
      res.setHeader('Content-Type', mime[path.extname(file)] || 'application/octet-stream')
      res.end(await fs.readFile(file))
    } catch { res.writeHead(404).end() }
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  const origin = `http://127.0.0.1:${server.address().port}`
  const errors = [], unexpected = [], mutations = [], checks = []
  let current = {
    id: 101, email: 'student.endpoint@example.test', priority: 0, loadFactor: 10,
    disabled: false, failureCount: 0, totalFailureCount: 0, refreshFailureCount: 0,
    isCurrent: false, expiresAt: null, authMethod: 'social', hasProfileArn: true,
    successCount: 0, totalCredits: 0, lastUsedAt: null, hasProxy: false, groups: [],
    metadata: {}, endpoint: 'ide', endpointOverride: null, subscriptionTitle: 'KIRO STUDENT',
  }
  let reject = false
  let browser
  try {
    browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_CHANNEL ? { channel: process.env.PLAYWRIGHT_CHANNEL } : {}) })
    const context = await browser.newContext({ viewport: { width: 1280, height: 1000 } })
    await context.addInitScript(() => {
      localStorage.setItem('adminApiKey', 'synthetic-group-test-key')
    })
    const groupNames = ['开发', '测试']
    await context.route('**/*', async route => {
      const request = route.request(), url = new URL(request.url())
      const send = (json, status = 200) => route.fulfill({ json, status })
      if (url.origin === 'https://fonts.googleapis.com') return route.fulfill({ contentType: 'text/css', body: '' })
      if (url.origin !== origin) { unexpected.push(request.url()); return route.abort() }
      if (!url.pathname.startsWith('/api/')) return route.continue()
      if (request.method() !== 'GET') {
        const body = request.postDataJSON()
        mutations.push({ path: url.pathname, body })
        if (url.pathname === '/api/admin/credentials/101' && request.method() === 'PUT') {
          if (reject) return send({ error: { message: 'synthetic save failure' } }, 500)
          if ('endpoint' in body) {
            current.endpointOverride = body.endpoint || null
            current.endpoint = body.endpoint || 'ide'
          }
          current.email = body.email
          return send({ success: true, message: 'updated' })
        }
        unexpected.push(`${request.method()} ${url.pathname}`); return route.abort()
      }
      switch (url.pathname) {
        case '/api/admin/credentials': return send({ total: 1, available: 1, currentId: 0, page: 1, pageSize: 12, stateCounts: { healthy: 1, total: 1 }, credentials: [current], metadataSchema: { type: 'object', properties: {}, additionalProperties: true } })
        case '/api/admin/groups': return send({ total: groupNames.length, groups: groupNames.map(name => ({ name, credentialCount: 0, keyCount: 0 })) })
        case '/api/admin/proxy-pool': return send({ proxies: [], total: 0 })
        case '/api/admin/config/global-proxy': return send({ proxyUrl: null })
        case '/api/admin/stats/by-credential': return send([])
        case '/api/admin/config/load-balancing': return send({ mode: 'balanced' })
        case '/api/admin/config/account-throttle': return send({ failover: true, cooldownSecs: 60 })
        case '/api/admin/config/self-heal': return send({ enabled: false, rules: [] })
        case '/api/admin/system/update/check': return send({ currentVersion: '0.9.8', latestVersion: '0.9.8', hasUpdate: false })
        case '/api/admin/traces/failure-stats':
        case '/api/admin/stats/credential-activity': return send({})
        default: unexpected.push(url.pathname); return send({}, 404)
      }
    })
    const page = await context.newPage()
    page.on('pageerror', error => errors.push(error.message))
    page.setDefaultTimeout(10000)
    await page.goto(`${origin}/admin/#/credentials`)
    await page.getByPlaceholder('搜索来源渠道 / 备注 / 邮箱').waitFor()
    const dialog = () => page.getByRole('dialog')
    const selector = () => dialog().locator('#credential-endpoint')
    async function open() {
      await page.getByRole('button', { name: '编辑凭据', exact: true }).click()
      await selector().waitFor()
    }
    async function choose(label) {
      await selector().click()
      await page.getByRole('menuitem', { name: label, exact: true }).click()
    }
    async function save() {
      const received = page.waitForRequest(request => request.method() === 'PUT' && new URL(request.url()).pathname.endsWith('/credentials/101'))
      await dialog().getByRole('button', { name: '保存', exact: true }).click()
      const body = (await received).postDataJSON()
      if (!reject) await dialog().waitFor({ state: 'hidden' })
      return body
    }
    await open()
    assert.equal(await selector().innerText(), '跟随全局默认')
    await dialog().getByText('Student 账号可选择 CLI 端点进行兼容性对照。', { exact: true }).waitFor()
    assert.equal('endpoint' in await save(), false)
    checks.push('unmodified inherited endpoint is omitted from the update')

    await open()
    await choose('CLI（1.4.121）')
    const cli = await save()
    assert.equal(cli.endpoint, 'cli')
    assert.equal('proxyId' in cli, false)
    assert.equal('proxyUrl' in cli, false)
    await open()
    assert.equal(await selector().innerText(), 'CLI（1.4.121）')
    checks.push('save CLI, refresh card and reopen preserves explicit selection and proxy')

    await choose('IDE')
    assert.equal((await save()).endpoint, 'ide')
    await open()
    assert.equal(await selector().innerText(), 'IDE')
    await choose('跟随全局默认')
    assert.equal((await save()).endpoint, '')
    checks.push('switch back to IDE and clear the override with an empty string')

    await open()
    await choose('CLI（1.4.121）')
    await dialog().getByRole('button', { name: '取消', exact: true }).click()
    await open()
    assert.equal(await selector().innerText(), '跟随全局默认')
    reject = true
    await choose('CLI（1.4.121）')
    await save()
    await dialog().getByRole('button', { name: '保存', exact: true }).waitFor({ state: 'visible' })
    assert.equal(current.endpointOverride, null)
    reject = false
    await page.keyboard.press('Escape')
    checks.push('cancel discards selection; failed save leaves the dialog open and account unchanged')

    // Legacy status API lacks endpointOverride. Editing an unrelated value must not pin the endpoint.
    delete current.endpointOverride
    current.endpoint = 'cli'
    await page.reload()
    await open()
    assert.equal(await selector().innerText(), 'CLI（1.4.121）')
    assert.equal('endpoint' in await save(), false)
    checks.push('older backend response preserves endpoint when untouched')

    current = { ...current, endpoint: 'ide', endpointOverride: null }
    await page.setViewportSize({ width: 390, height: 700 })
    await page.reload()
    await open()
    await choose('CLI（1.4.121）')
    const bounds = await dialog().getByRole('button', { name: '保存', exact: true }).boundingBox()
    assert.ok(bounds.y >= 0 && bounds.y + bounds.height <= 700, 'save button stays within the mobile viewport')
    await dialog().getByRole('button', { name: 'Metadata', exact: true }).click()
    await dialog().getByRole('button', { name: '基本', exact: true }).click()
    assert.equal(await selector().innerText(), 'CLI（1.4.121）')
    assert.equal((await save()).endpoint, 'cli')
    checks.push('mobile dialog remains usable and switching tabs retains the selected endpoint')
    assert.deepEqual(errors, [])
    assert.deepEqual(unexpected, [])
    console.log(JSON.stringify({ passed: checks.length, checks }, null, 2))
  } finally {
    await browser?.close()
    await new Promise(resolve => server.close(resolve))
  }
}
main().catch(error => { console.error(error); process.exitCode = 1 })
