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
    const simRow = page.getByRole('row', { name: /iroh-1to1-nat/ })
    await expect(simRow.getByRole('cell', { name: 'iroh-1to1-nat', exact: true })).toBeVisible()
    await simRow.getByRole('button', { name: 'open' }).click()

    await expect(page.getByRole('button', { name: 'perf' })).toBeVisible()
    const detailsSection = page.locator('.section', { hasText: 'transfer details' })
    await expect(detailsSection).toBeVisible()
    await expect(detailsSection.locator('tbody tr')).toHaveCount(1)
    const mbpsCells = detailsSection.locator('tbody tr td:nth-child(4)')
    await expect.poll(async () => {
      const values = await mbpsCells.allTextContents()
      return values
        .map((v) => Number.parseFloat(v))
        .filter((v) => Number.isFinite(v))
        .reduce((acc, v) => acc + v, 0)
    }).toBeGreaterThan(0)

    await page.getByRole('button', { name: 'logs' }).click()
    await expect(page.getByRole('button', { name: 'load log' })).toBeVisible()
    await page.getByRole('button', { name: 'load log' }).click()
    await expect.poll(async () => page.locator('.logs-content .log-entry').count()).toBeGreaterThan(0)

    await page.getByRole('button', { name: 'timeline' }).click()
    await expect(page.getByRole('cell', { name: /EndpointBound/ }).first()).toBeVisible()
    await expect(page.getByRole('cell', { name: /ConnectionClose/ }).first()).toBeVisible()
    await expect(page.getByRole('cell', { name: /path/ }).first()).toBeVisible()
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(workDir, { recursive: true, force: true })
  }
})
