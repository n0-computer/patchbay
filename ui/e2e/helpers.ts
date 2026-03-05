import { execFileSync } from 'node:child_process'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const THIS_DIR = path.dirname(fileURLToPath(import.meta.url))
export const REPO_ROOT = path.resolve(THIS_DIR, '../..')

/** Resolve the cargo target directory via `cargo metadata`. */
function resolveTargetDir(): string {
  const out = execFileSync('cargo', ['metadata', '--format-version=1', '--no-deps'], {
    cwd: REPO_ROOT,
    encoding: 'utf-8',
    timeout: 30_000,
  })
  const meta = JSON.parse(out) as { target_directory: string }
  return meta.target_directory
}

const TARGET_DIR = resolveTargetDir()
export const PATCHBAY_BIN = path.join(TARGET_DIR, 'debug', 'patchbay')

export async function waitForHttp(url: string, timeoutMs: number): Promise<void> {
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
