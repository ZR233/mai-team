import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import React from "react"
import ReactDOM from "react-dom/client"
import { BrowserRouter } from "react-router-dom"
import { Toaster } from "sonner"

import { App } from "@/app/app"
import { ProductEventController } from "@/events/product-event-controller"
import "@/index.css"

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 10_000, retry: 1, refetchOnWindowFocus: false },
    mutations: { retry: 0 },
  },
})

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <ProductEventController />
        <App />
        <Toaster position="bottom-right" />
      </BrowserRouter>
    </QueryClientProvider>
  </React.StrictMode>,
)
