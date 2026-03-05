import { expect, test } from '@playwright/test'
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { REPO_ROOT, PATCHBAY_BIN, waitForHttp } from './helpers'

const THIS_DIR = path.dirname(fileURLToPath(import.meta.url))
const PING_TOML = path.join(THIS_DIR, 'fixtures', 'ping-e2e.toml')
const IPERF_TOML = path.join(THIS_DIR, 'fixtures', 'iperf-e2e.toml')
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
      ['run', '--work-dir', workDir, PING_TOML],
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

    // Perf tab: should show latency column from ping results.
    await page.getByRole('button', { name: 'perf' }).click()
    await expect(page.getByText('ping-check')).toBeVisible({ timeout: 5_000 })
    await expect(page.getByText('Latency (ms)')).toBeVisible()
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(workDir, { recursive: true, force: true })
  }
})

test('multi-sim invocation shows grouped selector and combined results', async ({ page }) => {
  test.setTimeout(4 * 60 * 1000)
  const workDir = mkdtempSync(`${tmpdir()}/patchbay-runner-e2e-multi-`)
  let serveProc: ChildProcess | null = null
  try {
    // Run both sims in a single invocation.
    execFileSync(
      PATCHBAY_BIN,
      ['run', '--work-dir', workDir, PING_TOML, IPERF_TOML],
      {
        cwd: REPO_ROOT,
        stdio: 'inherit',
        env: process.env,
        timeout: 2 * 60 * 1000,
      },
    )

    // Start devtools server.
    serveProc = spawn(
      PATCHBAY_BIN,
      ['serve', workDir, '--bind', UI_BIND],
      { cwd: REPO_ROOT, stdio: 'inherit' },
    )
    await waitForHttp(UI_URL, 15_000)

    await page.goto(UI_URL)
    await expect(page.getByRole('heading', { name: 'patchbay' })).toBeVisible()

    // The selector should have an optgroup (invocation) with both sims.
    const selector = page.locator('select')
    await expect(selector).toBeVisible()
    await expect(selector.locator('optgroup')).toBeAttached()
    await expect(selector.locator('option', { hasText: 'ping-e2e' })).toBeAttached()
    await expect(selector.locator('option', { hasText: 'iperf-e2e' })).toBeAttached()

    // Select the "combined" option.
    const combinedOption = selector.locator('option', { hasText: 'combined' })
    await expect(combinedOption).toBeAttached()
    await selector.selectOption({ label: await combinedOption.innerText() })

    // Perf tab should show summary and detail tables with both sims.
    await expect(page.getByText('summary')).toBeVisible({ timeout: 5_000 })
    await expect(page.getByText('all steps')).toBeVisible()
    // Verify both sims appear in the summary table cells.
    await expect(page.getByRole('cell', { name: 'ping-e2e' }).first()).toBeVisible()
    await expect(page.getByRole('cell', { name: 'iperf-e2e' }).first()).toBeVisible()
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(workDir, { recursive: true, force: true })
  }
})

test('iperf sim shows perf results', async ({ page }) => {
  test.setTimeout(4 * 60 * 1000)
  const workDir = mkdtempSync(`${tmpdir()}/patchbay-runner-e2e-iperf-`)
  let serveProc: ChildProcess | null = null
  try {
    // Run the iperf sim.
    execFileSync(
      PATCHBAY_BIN,
      ['run', '--work-dir', workDir, IPERF_TOML],
      {
        cwd: REPO_ROOT,
        stdio: 'inherit',
        env: process.env,
        timeout: 2 * 60 * 1000,
      },
    )

    // Start devtools server.
    serveProc = spawn(
      PATCHBAY_BIN,
      ['serve', workDir, '--bind', UI_BIND],
      { cwd: REPO_ROOT, stdio: 'inherit' },
    )
    await waitForHttp(UI_URL, 15_000)

    await page.goto(UI_URL)
    await expect(page.getByRole('heading', { name: 'patchbay' })).toBeVisible()

    // Navigate to perf tab.
    await page.getByRole('button', { name: 'perf' }).click()
    await expect(page.getByText('iperf-client')).toBeVisible({ timeout: 5_000 })
    await expect(page.getByText('Down MB/s')).toBeVisible()
  } finally {
    if (serveProc && !serveProc.killed) {
      serveProc.kill('SIGTERM')
    }
    rmSync(workDir, { recursive: true, force: true })
  }
})
