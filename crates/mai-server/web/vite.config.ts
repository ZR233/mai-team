import path from "node:path"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vitest/config"

const apiTarget = process.env.MAI_WEB_API_TARGET ?? "http://127.0.0.1:8080"

function apiProxy({ spaNavigation = false } = {}) {
  return {
    target: apiTarget,
    bypass(request: { headers: { accept?: string } }) {
      return spaNavigation && request.headers.accept?.includes("text/html")
        ? "/index.html"
        : undefined
    },
  }
}

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  server: {
    proxy: {
      "/agent-config": apiProxy(),
      "/agents": apiProxy(),
      "/environments": apiProxy(),
      "/events": apiProxy(),
      "/git": apiProxy(),
      "/github": apiProxy(),
      "/mcp-servers": apiProxy(),
      "/provider-catalog": apiProxy(),
      "/providers": apiProxy({ spaNavigation: true }),
      "/projects": apiProxy({ spaNavigation: true }),
      "/relay": apiProxy(),
      "/runtime": apiProxy(),
      "/sessions": apiProxy(),
      "/settings": apiProxy({ spaNavigation: true }),
      "/skills": apiProxy(),
      "/tasks": apiProxy({ spaNavigation: true }),
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks(id) {
          const packageName = nodePackageName(id)
          if (!packageName) return undefined
          if (["react", "react-dom", "scheduler", "react-router", "react-router-dom"].includes(packageName)) return "react"
          if (["@tanstack/react-query", "zustand"].includes(packageName)) return "state"
          if (["marked", "dompurify", "highlight.js"].includes(packageName)) return "content"
          if (packageName === "radix-ui" || packageName.startsWith("@radix-ui/") || packageName === "lucide-react" || packageName === "sonner") return "ui"
          return undefined
        },
      },
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    css: true,
  },
})

function nodePackageName(id: string) {
  const marker = "/node_modules/"
  const index = id.lastIndexOf(marker)
  if (index < 0) return null
  const path = id.slice(index + marker.length)
  const parts = path.split("/")
  return parts[0]?.startsWith("@") ? `${parts[0]}/${parts[1]}` : parts[0]
}
