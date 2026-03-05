import { expect, test } from '@playwright/test'
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { REPO_ROOT, PATCHBAY_BIN, waitForHttp } from './helpers'

const THIS_DIR = path.dirname(fileURLToPath(import.meta.url))
const SIM_TOML = path.join(THIS_DIR, 'fixtures', 'ping-e2e.toml')
const UI_BIND = '127.0.0.1:7432'
const UI_URL = `http://${UI_BIND}/`

test('runner sim produces viewable UI output', async ({ page }) => {
  test.setTimeout(4 * 60 * 1000)
  const workDir = mkdtempSync(`${tmpdir()}/patchbay-runner-e2e-`)
  let serveProc: ChildProcess | null = null
  try {
    // Step 1: Run the sim.
    execFileSync(
      PATCHBAY_BIN,
      ['run', '--work-dir', workDir, SIM_TOML],
      {
        cwd: REPO_ROOT,
        stdio: 'inherit',
        env: process.env,
        timeout: 2 * 60 * 1000,
      },
    )

    // Step 2: Start the devtools server.
    serveProc = spawn(
      PATCHBAY_BIN,
      ['serve', workDir, '--bind', UI_BIND],
      { cwd: REPO_ROOT, stdio: 'inherit' },
    )
    await waitForHttp(UI_URL, 15_000)

    // Step 3: Verify the UI loads and shows the run.
    await page.goto(UI_URL)
    await expect(page.getByRole('heading', { name: 'patchbay' })).toBeVisible()

    const selector = page.locator('select')
    await expect(selector).toBeVisible()
    await expect(selector.locator('option', { hasText: 'ping-e2e' })).toBeAttached()

    // Topology tab should show the router and devices.
    await expect(page.getByText('dc')).toBeVisible({ timeout: 10_000 })
    await expect(page.getByText('sender')).toBeVisible()
    await expect(page.getByText('receiver')).toBeVisible()

    // Logs tab: events.jsonl should show lab events.
    await page.getByRole('button', { name: 'logs' }).click()
    await expect(page.getByText('events.jsonl').first()).toBeVisible({ timeout: 5_000 })
    await page.getByText('events.jsonl').first().click()
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
