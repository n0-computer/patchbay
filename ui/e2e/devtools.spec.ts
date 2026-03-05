import { expect, test } from '@playwright/test'
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { REPO_ROOT, PATCHBAY_BIN, waitForHttp } from './helpers'

const DEVTOOLS_BIND = '127.0.0.1:7431'
const DEVTOOLS_URL = `http://${DEVTOOLS_BIND}`

test('devtools ui shows all views', async ({ page }) => {
  test.setTimeout(4 * 60 * 1000)
  const outdir = mkdtempSync(`${tmpdir()}/patchbay-e2e-devtools-`)
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
    await expect(selector.locator('option', { hasText: 'e2e-test' })).toBeAttached()

    // Step 4: Verify topology tab shows router and device nodes (default tab).
    await expect(page.getByText('dc')).toBeVisible({ timeout: 10_000 })
    await expect(page.getByText('home')).toBeVisible()
    await expect(page.getByText('client')).toBeVisible()
    await expect(page.getByText('server')).toBeVisible()

    // Step 5: Switch to logs tab and verify events.jsonl is listed (lab_events kind).
    await page.getByRole('button', { name: 'logs' }).click()
    await expect(page.getByText('events.jsonl').first()).toBeVisible({ timeout: 5_000 })

    // Click events.jsonl and verify lab events render in the table.
    await page.getByText('events.jsonl').first().click()
    const eventsTable = page.locator('table tbody tr')
    await expect(eventsTable.first()).toBeVisible({ timeout: 5_000 })
    await expect(page.getByText('router_added').first()).toBeVisible()
    await expect(page.getByText('device_added').first()).toBeVisible()

    // Step 6: Verify tracing log files are listed in the sidebar.
    await expect(page.getByText('device.client.tracing.jsonl').first()).toBeVisible({ timeout: 5_000 })
    await expect(page.getByText('device.server.tracing.jsonl').first()).toBeVisible()
    await page.getByText('device.client.tracing.jsonl').first().click()

    // Verify rendered tracing log lines have all expected parts.
    const logEntry = page.locator('.log-entry').first()
    await expect(logEntry).toBeVisible({ timeout: 5_000 })
    await expect(logEntry.locator('.log-ts')).toBeVisible()
    await expect(logEntry.locator('[class*="level-"]')).toBeVisible()
    await expect(logEntry.locator('.log-target')).toBeVisible()
    const nestedSpanEntry = page.locator('.log-entry', {
      hasText: 's1{z=3}:s2{y=2}: foo: x=1',
    })
    await expect(nestedSpanEntry).toBeVisible({ timeout: 10_000 })

    // Step 7: Switch to the timeline tab and verify events.
    await page.getByRole('button', { name: 'timeline' }).click()
    await expect(page.getByText('TcpRoundtripComplete')).toBeVisible({ timeout: 10_000 })
    await expect(page.getByText('TcpEchoStarted')).toBeVisible({ timeout: 5_000 })

    // Step 8: Jump to log from timeline.
    await page.locator('.timeline-event-cell.has-event', { hasText: 'TcpEchoStarted' }).first().click()
    const jumpBtn = page.getByRole('button', { name: 'jump to logs' })
    await expect(jumpBtn).toBeVisible({ timeout: 2_000 })
    await jumpBtn.click()
    await expect(page.locator('.tab-btn.active', { hasText: 'logs' })).toBeVisible()
    await expect(page.locator('text=jump:')).toBeVisible({ timeout: 5_000 })
    await expect(page.locator('.log-entry.jump-hit')).toBeVisible({ timeout: 5_000 })
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(outdir, { recursive: true, force: true })
  }
})
