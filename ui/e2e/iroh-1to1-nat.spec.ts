import { expect, test } from '@playwright/test'
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const THIS_FILE = fileURLToPath(import.meta.url)
const THIS_DIR = path.dirname(THIS_FILE)
const REPO_ROOT = path.resolve(THIS_DIR, '../..')
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

test('ui shows iperf run results', async ({ page }) => {
  test.setTimeout(8 * 60 * 1000)
  const workDir = mkdtempSync(path.join(tmpdir(), 'netsim-e2e-'))
  let serveProc: ChildProcess | null = null
  try {
    execFileSync(
      'cargo',
      ['run', '--bin', 'netsim', '--', 'run', '--work-dir', workDir, './iroh-integration/netsim/sims/iperf-1to1-public.toml'],
      { cwd: REPO_ROOT, stdio: 'inherit' },
    )

    serveProc = spawn(
      'cargo',
      ['run', '--bin', 'netsim', '--', 'serve', '--work-dir', workDir, '--bind', UI_BIND],
      { cwd: REPO_ROOT, stdio: 'ignore' },
    )
    await waitForHttp(UI_URL, 30_000)

    await page.goto(UI_URL)
    await expect(page.getByRole('heading', { name: 'netsim' })).toBeVisible()
    const simRow = page.getByRole('row', { name: /iperf-1to1-public-baseline/ })
    await expect(
      simRow.getByRole('cell', { name: 'iperf-1to1-public-baseline', exact: true }),
    ).toBeVisible()
    await simRow.getByRole('button', { name: 'open' }).click()

    await expect(page.getByRole('button', { name: 'perf' })).toBeVisible()
    const detailsSection = page.locator('.section', { hasText: 'iperf' })
    await expect(detailsSection).toBeVisible()
    await expect(detailsSection.locator('tbody tr')).toHaveCount(1)
    await expect(detailsSection.getByRole('cell', { name: 'iperf-baseline', exact: true })).toBeVisible()

    await page.getByRole('button', { name: 'timeline' }).click()
    await expect(page.getByText('no timeline events yet')).toBeVisible()
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(workDir, { recursive: true, force: true })
  }
})
