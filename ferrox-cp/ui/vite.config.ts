import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': 'http://localhost:9090',
      '/token': 'http://localhost:9090',
      '/.well-known': 'http://localhost:9090',
      '/healthz': 'http://localhost:9090',
    },
  },
})
