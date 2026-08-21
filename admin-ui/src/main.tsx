import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import App from './App'
import { AnchoredToastProvider, ToastProvider } from '@/components/ui/toast'
import { TooltipProvider } from '@/components/ui/tooltip'
import { LanguageProvider } from '@/lib/i18n'
import { initTheme } from '@/lib/theme'
import './index.css'

// 渲染前先上色：系统 / 浅色 / 深色三态，见 lib/theme.ts。
initTheme()

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 5000, refetchOnWindowFocus: false },
  },
})

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <LanguageProvider>
      <QueryClientProvider client={queryClient}>
        <ToastProvider position="top-right">
          <TooltipProvider>
            <AnchoredToastProvider>
              <div className="relative isolate min-h-svh">
                <App />
              </div>
            </AnchoredToastProvider>
          </TooltipProvider>
        </ToastProvider>
      </QueryClientProvider>
    </LanguageProvider>
  </React.StrictMode>,
)
