import { expect, test } from '@playwright/test'
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

const REPO_ROOT = path.resolve(__dirname, '../..')
const UI_BIND = '127.0.0.1:7429'
const UI_URL = `http://${UI_BIND}/`

async function waitForHttp(url: string, timeoutMs: number): Promise<void> {
  const start = Date.now()
  while (Date.now() - start < timeoutMs) {
    try {
      const res = await fetch(url)
      if (res.ok) return
    } catch {
      // retry
    }
    await new Promise((resolve) => setTimeout(resolve, 200))
  }
  throw new Error(`timed out waiting for ${url}`)
}

test('ui shows iroh-1to1-nat run results', async ({ page }) => {
  test.setTimeout(8 * 60 * 1000)
  const workDir = mkdtempSync(path.join(tmpdir(), 'netsim-e2e-'))
  let serveProc: ChildProcess | null = null
  try {
    execFileSync(
      'cargo',
      ['run', '--', 'run', '--work-dir', workDir, './iroh-integration/sims/iroh-1to1-nat.toml'],
      { cwd: REPO_ROOT, stdio: 'inherit' },
    )

    serveProc = spawn(
      'cargo',
      ['run', '--', 'serve', '--work-dir', workDir, '--bind', UI_BIND],
      { cwd: REPO_ROOT, stdio: 'ignore' },
    )
    await waitForHttp(UI_URL, 30_000)

    await page.goto(UI_URL)
    await expect(page.getByRole('heading', { name: 'netsim' })).toBeVisible()
    await expect(page.locator('text=iroh-1to1-nat')).toBeVisible()
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(workDir, { recursive: true, force: true })
  }
})
