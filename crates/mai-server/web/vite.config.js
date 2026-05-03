import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

const apiTarget = process.env.MAI_WEB_API_TARGET || 'http://127.0.0.1:8080'

export default defineConfig({
  plugins: [vue()],
  server: {
    proxy: {
      '/agent-config': apiTarget,
      '/agents': apiTarget,
      '/events': apiTarget,
      '/provider-presets': apiTarget,
      '/providers': apiTarget
    }
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true
  }
})
