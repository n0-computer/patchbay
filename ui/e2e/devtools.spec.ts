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
const DEVTOOLS_BIND = '127.0.0.1:7431'
const DEVTOOLS_URL = `http://${DEVTOOLS_BIND}`

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

test('devtools ui shows all views', async ({ page }) => {
  test.setTimeout(4 * 60 * 1000)
  const outdir = mkdtempSync(path.join(tmpdir(), 'patchbay-e2e-devtools-'))
  let serveProc: ChildProcess | null = null

  try {
    // Step 1: Run the Rust integration test to create lab output.
    execFileSync(
      'cargo',
      ['test', '-p', 'patchbay', 'simple_lab_for_e2e', '--', '--ignored', '--nocapture'],
      {
        cwd: REPO_ROOT,
        stdio: 'inherit',
        env: { ...process.env, PATCHBAY_OUTDIR: outdir },
        timeout: 3 * 60 * 1000,
      },
    )

    // Step 2: Start the devtools server.
    serveProc = spawn(
      PATCHBAY_BIN,
      ['serve', outdir, '--bind', DEVTOOLS_BIND],
      { cwd: REPO_ROOT, stdio: 'inherit' },
    )
    await waitForHttp(`${DEVTOOLS_URL}/api/runs`, 60_000)

    // Step 3: Open the UI.
    await page.goto(DEVTOOLS_URL)

    // Verify the topbar shows "patchbay" heading.
    await expect(page.getByRole('heading', { name: 'patchbay' })).toBeVisible()

    // The run selector should have an entry containing "e2e-test".
    const selector = page.locator('select')
    await expect(selector).toBeVisible()
    // Select the e2e run (should be auto-selected as it's the only one).
    await expect(selector.locator('option', { hasText: 'e2e-test' })).toBeAttached()

    // Step 4: Verify topology tab shows router and device nodes (default tab).
    // Use text matching since ReactFlow wraps nodes in its own DOM structure.
    await expect(page.getByText('dc')).toBeVisible({ timeout: 10_000 })
    await expect(page.getByText('home')).toBeVisible()
    await expect(page.getByText('client')).toBeVisible()
    await expect(page.getByText('server')).toBeVisible()

    // Step 5: Switch to the events tab.
    await page.getByRole('button', { name: 'events' }).click()
    const eventsTable = page.locator('table tbody tr')
    await expect(eventsTable.first()).toBeVisible({ timeout: 5_000 })
    await expect(page.getByText('router_added').first()).toBeVisible()
    await expect(page.getByText('device_added').first()).toBeVisible()

    // Step 6: Switch to the logs tab and verify log files are listed.
    await page.getByRole('button', { name: 'logs' }).click()
    // Should see tracing files in the sidebar (flat naming: device.client.tracing.jsonl).
    await expect(page.getByText('device.client.tracing.jsonl').first()).toBeVisible({ timeout: 5_000 })
    await expect(page.getByText('device.server.tracing.jsonl').first()).toBeVisible()
    await page.getByText('device.client.tracing.jsonl').first().click()

    // Verify rendered tracing log lines have all expected parts:
    // timestamp, level, target, and message, plus nested parent spans.
    const logEntry = page.locator('.log-entry').first()
    await expect(logEntry).toBeVisible({ timeout: 5_000 })
    await expect(logEntry.locator('.log-ts')).toBeVisible()        // timestamp
    await expect(logEntry.locator('[class*="level-"]')).toBeVisible() // level badge
    await expect(logEntry.locator('.log-target')).toBeVisible()    // target
    const nestedSpanEntry = page.locator('.log-entry', {
      hasText: 's1{z=3}:s2{y=2}: foo: x=1',
    })
    await expect(nestedSpanEntry).toBeVisible({ timeout: 10_000 })

    // Step 7: Switch to the timeline tab and verify events.
    await page.getByRole('button', { name: 'timeline' }).click()
    // Should see extracted events from .events.jsonl files.
    await expect(page.getByText('TcpRoundtripComplete')).toBeVisible({ timeout: 10_000 })
    await expect(page.getByText('TcpEchoStarted')).toBeVisible({ timeout: 5_000 })

    // Step 8: Jump to log from timeline — click an event, then "jump to logs".
    await page.locator('.timeline-event-cell.has-event', { hasText: 'TcpEchoStarted' }).first().click()
    const jumpBtn = page.getByRole('button', { name: 'jump to logs' })
    await expect(jumpBtn).toBeVisible({ timeout: 2_000 })
    await jumpBtn.click()
    // Should switch to logs tab, select the tracing file, and show jump indicator.
    await expect(page.locator('.tab-btn.active', { hasText: 'logs' })).toBeVisible()
    await expect(page.locator('text=jump:')).toBeVisible({ timeout: 5_000 })
    // The jump target should highlight a log entry.
    await expect(page.locator('.log-entry.jump-hit')).toBeVisible({ timeout: 5_000 })
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(outdir, { recursive: true, force: true })
  }
})
