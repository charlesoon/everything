import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

function startupRequestLogger() {
  const startedAt = Date.now();
  const elapsed = () => Date.now() - startedAt;
  const shouldLog = (url = '') => (
    url === '/' ||
    url.startsWith('/index.html') ||
    url.startsWith('/src/main.js') ||
    url.startsWith('/src/App.svelte') ||
    url.startsWith('/@vite/client')
  );

  return {
    name: 'startup-request-logger',
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const url = req.url || '';
        if (!shouldLog(url)) {
          next();
          return;
        }
        delete req.headers['if-none-match'];
        delete req.headers['if-modified-since'];
        res.setHeader('Cache-Control', 'no-store');
        const reqStarted = Date.now();
        console.log(`[vite/startup +${elapsed()}ms] --> ${req.method} ${url}`);
        res.on('finish', () => {
          const duration = Date.now() - reqStarted;
          console.log(`[vite/startup +${elapsed()}ms] <-- ${res.statusCode} ${req.method} ${url} (${duration}ms)`);
        });
        next();
      });
    }
  };
}

export default defineConfig({
  plugins: [svelte(), startupRequestLogger()],
  clearScreen: false,
  server: {
    host: '127.0.0.1',
    port: 1420,
    strictPort: true,
    warmup: {
      clientFiles: ['./src/main.js', './src/App.svelte']
    },
    watch: {
      ignored: ['**/src-tauri/target/**', '**/src-tauri/target-test/**']
    }
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: process.env.TAURI_ENV_PLATFORM === 'windows' ? 'chrome105' : 'safari13',
    minify: !process.env.TAURI_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_DEBUG
  }
});
