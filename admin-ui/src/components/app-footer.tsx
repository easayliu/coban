export function AppFooter() {
  return (
    <footer className="app-footer-bar mt-auto border-t border-border/70 bg-transparent">
      <div className="page-frame flex items-center justify-end py-4 text-xs text-muted-foreground">
        <span className="tabular-nums">v{__APP_VERSION__}</span>
      </div>
    </footer>
  )
}
