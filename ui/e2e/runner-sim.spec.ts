import { expect, test } from '@playwright/test'
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const THIS_DIR = path.dirname(fileURLToPath(import.meta.url))
const REPO_ROOT = path.resolve(THIS_DIR, '../..')
const TARGET_DIR = process.env.CARGO_TARGET_DIR ?? path.join(REPO_ROOT, 'target')
const PATCHBAY_BIN = path.join(TARGET_DIR, 'debug', 'patchbay')
const SIM_TOML = path.join(THIS_DIR, 'fixtures', 'ping-e2e.toml')
const UI_BIND = '127.0.0.1:7432'
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

test('runner sim produces viewable UI output', async ({ page }) => {
  test.setTimeout(4 * 60 * 1000)
  const workDir = mkdtempSync(path.join(tmpdir(), 'patchbay-runner-e2e-'))
  let serveProc: ChildProcess | null = null
  try {
    // Step 1: Run the sim with PATCHBAY_OUTDIR so the lab writes events.jsonl.
    execFileSync(
      PATCHBAY_BIN,
      ['run', '--work-dir', workDir, SIM_TOML],
      {
        cwd: REPO_ROOT,
        stdio: 'inherit',
        env: { ...process.env, PATCHBAY_OUTDIR: workDir },
        timeout: 2 * 60 * 1000,
      },
    )

    // Step 2: Start the devtools server pointing at the work directory.
    serveProc = spawn(
      PATCHBAY_BIN,
      ['serve', workDir, '--bind', UI_BIND],
      { cwd: REPO_ROOT, stdio: 'inherit' },
    )
    await waitForHttp(UI_URL, 15_000)

    // Step 3: Verify the UI loads and shows the run.
    await page.goto(UI_URL)
    await expect(page.getByRole('heading', { name: 'patchbay' })).toBeVisible()

    // The run selector should have the "ping-e2e" run we just produced.
    const selector = page.locator('select')
    await expect(selector).toBeVisible()
    await expect(selector.locator('option', { hasText: 'ping-e2e' })).toBeAttached()

    // Topology tab should show the router and devices from our sim.
    await expect(page.getByText('dc')).toBeVisible({ timeout: 10_000 })
    await expect(page.getByText('sender')).toBeVisible()
    await expect(page.getByText('receiver')).toBeVisible()

    // Events tab should show lab setup events.
    await page.getByRole('button', { name: 'events' }).click()
    const eventsTable = page.locator('table tbody tr')
    await expect(eventsTable.first()).toBeVisible({ timeout: 5_000 })
    await expect(page.getByText('router_added').first()).toBeVisible()
    await expect(page.getByText('device_added').first()).toBeVisible()
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(workDir, { recursive: true, force: true })
  }
})
