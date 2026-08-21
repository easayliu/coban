import { defineConfig, type PluginOption } from 'vite'
import react from '@vitejs/plugin-react-swc'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'
import { readFileSync } from 'node:fs'

/**
 * 版本号取自 Cargo.toml（唯一真源），读不到就退回 `dev`。
 *
 * **必须容错**：这里抛异常会让 vite 连配置都加载不了、整个前端构建当场挂掉，而它只是页脚上
 * 的一个字符串。Docker 的前端阶段一度就只拷 admin-ui、没有 ../Cargo.toml，于是整包构建失败
 * （v0.2.25）；那边已补上 COPY，这条 catch 是防下一个「构建上下文里没有它」的场景。
 */
function readAppVersion(): string {
  try {
    const manifest = readFileSync(path.resolve(__dirname, '../Cargo.toml'), 'utf8')
    return manifest.match(/^version\s*=\s*"([^"]+)"/m)?.[1] ?? 'dev'
  } catch {
    return 'dev'
  }
}

const appVersion = readAppVersion()

/**
 * 剥掉 Fontsource 的 `.woff` 回退，只留 woff2。
 *
 * woff2 从 2016 年起就是全平台可用（唯一的例外 IE11 早已不在支持范围），而这些字体文件
 * 会跟着 dist 一起被 rust-embed 编译进 coban 二进制——多带一份等价格式等于凭空给每个
 * 发行版塞进 150 KB 永远不会被请求的字节。
 */
function dropWoff1(): PluginOption {
  return {
    name: 'coban-drop-woff1',
    transform(code, id) {
      if (!id.includes('.css')) return null
      // 此处 url() 已被 Vite 换成 __VITE_ASSET__ 占位符，认不了后缀，靠 format("woff")
      // 精确定位（woff2 的引号内是 woff2，不会被这条误伤）。
      return { code: code.replace(/,\s*url\([^)]*\)\s*format\(["']woff["']\)/g, ''), map: null }
    },
    // 去掉 CSS 引用还不够：资源在更早的阶段就已登记，不在这里删会照样落进 dist。
    generateBundle(_options, bundle) {
      for (const name of Object.keys(bundle)) {
        if (name.endsWith('.woff')) delete bundle[name]
      }
    },
  }
}

// coban 网页在根路径 `/` 提供服务；开发时 /api 代理到本地 coban 后端。
export default defineConfig({
  plugins: [
    react(),
    tailwindcss(),
    dropWoff1(),
  ],
  base: '/',
  define: {
    __APP_VERSION__: JSON.stringify(appVersion),
  },
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:4700',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
})
