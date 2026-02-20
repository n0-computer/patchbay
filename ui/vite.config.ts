import { defineConfig, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'
import { viteSingleFile } from 'vite-plugin-singlefile'
import fs from 'fs'
import path from 'path'
import { createReadStream } from 'fs'

// Default work dir: repo root / .netsim-work  (ui/ is one level inside repo root)
// Override: NETSIMS=/absolute/or/relative/path npm run dev
const DEFAULT_WORK = path.resolve(__dirname, '../.netsim-work')
const workRoot = path.resolve(process.env.NETSIMS ?? DEFAULT_WORK)

const MIME: Record<string, string> = {
  '.json': 'application/json',
  '.qlog': 'application/json',
  '.log':  'text/plain; charset=utf-8',
  '.txt':  'text/plain; charset=utf-8',
  '.md':   'text/plain; charset=utf-8',
  '.html': 'text/html',
}

/** Serves files from workRoot and exposes /__netsim/runs JSON endpoint. */
function netsimPlugin(): Plugin {
  return {
    name: 'netsim-serve',
    configureServer(server) {
      // ── /__netsim/runs → JSON list of run subdirs ──────────────────────────
      server.middlewares.use('/__netsim/runs', (_req, res) => {
        try {
          const entries = fs.readdirSync(workRoot, { withFileTypes: true })
          const runs = entries
            .filter(e => e.isDirectory() && !e.name.startsWith('.') && e.name !== 'latest')
            .map(e => e.name)
            .sort()
            .reverse() // newest first
          res.setHeader('Content-Type', 'application/json')
          res.end(JSON.stringify({ workRoot, runs }))
        } catch (e) {
          res.statusCode = 500
          res.end(JSON.stringify({ error: String(e) }))
        }
      })

      // ── Static files from workRoot ─────────────────────────────────────────
      server.middlewares.use((req, res, next) => {
        const url = req.url ?? '/'
        // Let vite handle its own internals
        if (
          url.startsWith('/@') ||
          url.startsWith('/src') ||
          url.startsWith('/node_modules') ||
          url.startsWith('/__netsim') ||
          url === '/' ||
          url.startsWith('/index.html')
        ) return next()

        const decoded = decodeURIComponent(url.split('?')[0])
        const filePath = path.join(workRoot, decoded)

        // Prevent path traversal outside workRoot
        if (!filePath.startsWith(workRoot + path.sep) && filePath !== workRoot) {
          return next()
        }

        try {
          const stat = fs.statSync(filePath)
          if (!stat.isFile()) return next()
          const ext = path.extname(filePath).toLowerCase()
          res.setHeader('Content-Type', MIME[ext] ?? 'application/octet-stream')
          res.setHeader('Content-Length', stat.size)
          createReadStream(filePath).pipe(res as unknown as NodeJS.WritableStream)
        } catch {
          next()
        }
      })

      console.log(`\n  netsim work root: \x1b[36m${workRoot}\x1b[0m\n`)
    },
  }
}

export default defineConfig({
  plugins: [react(), netsimPlugin(), viteSingleFile()],
  build: {
    target: 'esnext',
    assetsInlineLimit: 100_000_000,
    cssCodeSplit: false,
    rollupOptions: {
      output: { inlineDynamicImports: true },
    },
  },
})
