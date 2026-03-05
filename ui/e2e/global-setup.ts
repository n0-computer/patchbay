import { execFileSync } from 'node:child_process'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const THIS_DIR = path.dirname(fileURLToPath(import.meta.url))
const UI_DIR = path.resolve(THIS_DIR, '..')
const REPO_ROOT = path.resolve(UI_DIR, '..')

export default function globalSetup() {
  console.log('[setup] building UI...')
  execFileSync('npm', ['run', 'build'], {
    cwd: UI_DIR,
    stdio: 'inherit',
    timeout: 60_000,
  })

  console.log('[setup] building cargo workspace...')
  execFileSync('cargo', ['build', '-p', 'patchbay-runner', '-p', 'patchbay-server'], {
    cwd: REPO_ROOT,
    stdio: 'inherit',
    timeout: 5 * 60_000,
  })
}
