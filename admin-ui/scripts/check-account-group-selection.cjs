// Run after building admin-ui. All APIs and OAuth navigation are mocked locally.
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
  let browser
  try {
    browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_CHANNEL ? { channel: process.env.PLAYWRIGHT_CHANNEL } : {}) })
    const context = await browser.newContext({ viewport: { width: 1280, height: 1000 } })
    await context.addInitScript(() => {
      localStorage.setItem('adminApiKey', 'synthetic-group-test-key')
      window.open = () => null
    })
    let groupNames = ['开发', '测试']
    await context.route('**/*', async route => {
      const request = route.request(), url = new URL(request.url())
      const send = (json, status = 200) => route.fulfill({ json, status })
      if (url.origin === 'https://fonts.googleapis.com') return route.fulfill({ contentType: 'text/css', body: '' })
      if (url.origin !== origin) { unexpected.push(request.url()); return route.abort() }
      if (!url.pathname.startsWith('/api/')) return route.continue()
      if (request.method() !== 'GET') {
        const body = request.postDataJSON()
        mutations.push({ path: url.pathname, body })
        if (url.pathname.endsWith('/auth/social/start')) return send({ sessionId: 'fixture', portalUrl: `${origin}/fixture-oauth`, expiresAt: '2099-01-01T00:00:00Z' })
        if (url.pathname.endsWith('/auth/idc/start')) return send({ sessionId: 'fixture', userCode: 'TEST', verificationUri: `${origin}/fixture-oauth`, pollInterval: 3600 })
        if (url.pathname.endsWith('/auth/social/poll/fixture')) return send({ status: 'pending' })
        if (url.pathname.endsWith('/credentials/batch-import')) {
          const events = body.credentials.map((_, index) => ({ index, status: 'imported', credentialId: 200 + index }))
          events.push({ status: 'summary', summary: { imported: events.length, verified: 0, failed: 0, duplicate: 0, total: events.length } })
          return route.fulfill({ contentType: 'text/event-stream', body: events.map(event => `data: ${JSON.stringify(event)}\n\n`).join('') })
        }
        if (url.pathname === '/api/admin/credentials') return send({ id: 200, message: 'added' })
        unexpected.push(`${request.method()} ${url.pathname}`); return route.abort()
      }
      switch (url.pathname) {
        case '/api/admin/credentials': return send({ total: 0, available: 0, currentId: 0, page: 1, pageSize: 12, stateCounts: {}, credentials: [], metadataSchema: { type: 'object', properties: {}, additionalProperties: true } })
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
    async function open(menu) {
      if (menu === 'manual') await page.getByRole('button', { name: '添加凭据', exact: true }).click()
      else {
        await page.getByRole('button', { name: '登录 / 导入 / 导出', exact: true }).click()
        await page.getByRole('menuitem', { name: menu, exact: true }).click()
      }
      await page.getByRole('group', { name: '账号分组', exact: true }).waitFor()
    }
    const dialog = () => page.getByRole('dialog')
    async function selectGroups() {
      await page.getByRole('group', { name: '账号分组', exact: true }).getByRole('button', { name: '选择分组', exact: true }).click()
      await page.getByRole('menuitem', { name: '开发', exact: true }).click()
      await page.getByRole('menuitem', { name: '测试', exact: true }).click()
      await page.keyboard.press('Escape')
      assert.match(await dialog().innerText(), /已选：开发、测试/)
    }
    async function close() {
      await page.keyboard.press('Escape')
      await dialog().waitFor({ state: 'hidden' })
    }
    async function submit(button, suffix) {
      const received = page.waitForRequest(request => request.method() === 'POST' && new URL(request.url()).pathname.endsWith(suffix))
      await dialog().getByRole('button', { name: button, exact: true }).click()
      return (await received).postDataJSON()
    }
    for (const menu of ['Kiro 账号登录', 'AWS SSO (IdC) 登录', 'Enterprise (IAM Identity Center) 登录']) {
      await open(menu)
      await selectGroups()
      if (menu.startsWith('Enterprise')) await dialog().locator('#idc-start-url').fill('https://example.awsapps.com/start')
      const social = menu.startsWith('Kiro')
      const body = await submit('发起登录', social ? '/auth/social/start' : '/auth/idc/start')
      assert.deepEqual(body.groups, ['开发', '测试'])
      assert.equal(body.proxyUrl, '', 'global proxy selection must remain unchanged')
      await dialog().getByText(social ? '正在等待登录完成…' : 'TEST', { exact: true }).waitFor()
      await close()
      await open(menu)
      await dialog().getByRole('button', { name: '选择分组', exact: true }).waitFor()
      await close()
      checks.push(`${menu}: submit multiple groups before authorization and reset on close`)
    }
    await open('manual')
    await selectGroups()
    await dialog().locator('#authMethod').click()
    await page.getByRole('menuitem', { name: 'API Key', exact: true }).click()
    await dialog().locator('#kiroApiKey').fill('ksk_offline_group_fixture')
    assert.deepEqual((await submit('添加', '/credentials')).groups, ['开发', '测试'])
    await dialog().waitFor({ state: 'hidden' })
    checks.push('manual add submits groups together with the credential')
    for (const menu of ['批量导入', 'Kiro Account Manager 导入']) {
      const kam = menu !== '批量导入'
      const records = kam
        ? [{ refreshToken: 'fixture-flat', groups: ['文件分组'] }, { credentials: { refreshToken: 'fixture-nested', groups: ['嵌套分组'] } }]
        : [{ kiroApiKey: 'ksk_fixture', groups: ['文件分组'] }, { refreshToken: 'fixture-oauth', groups: ['另一个分组'] }]
      for (const selected of [false, true]) {
        await open(menu)
        if (selected) await selectGroups()
        await dialog().locator('textarea').fill(JSON.stringify(records))
        const body = await submit('直接导入（不验活）', '/credentials/batch-import')
        assert.deepEqual(body.credentials.map(c => c.groups), selected ? [['开发', '测试'], ['开发', '测试']] : [['文件分组'], [kam ? '嵌套分组' : '另一个分组']])
        await dialog().getByRole('button', { name: '关闭', exact: true }).waitFor()
        await close()
      }
      checks.push(`${menu}: preserve file groups by default and override all records when selected`)
    }
    // A blank group list must not block adding accounts; the management link remains available.
    groupNames = []
    await page.reload()
    await open('manual')
    await dialog().getByRole('group', { name: '账号分组', exact: true }).getByRole('link', { name: '分组管理', exact: true }).waitFor()
    await close()
    checks.push('empty registry provides a link to group management')
    assert.deepEqual(errors, [])
    assert.deepEqual(unexpected, [])
    console.log(JSON.stringify({ passed: checks.length, checks, apiMutations: mutations.length }, null, 2))
  } finally {
    await browser?.close()
    await new Promise(resolve => server.close(resolve))
  }
}
main().catch(error => { console.error(error); process.exitCode = 1 })
