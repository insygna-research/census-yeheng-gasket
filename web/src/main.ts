import { createApp } from 'vue'
import { createPinia } from 'pinia'
import '@fontsource/inter/400.css'
import '@fontsource/inter/500.css'
import '@fontsource/inter/600.css'
import './styles/index.less'
import { initStorage } from './lib/storage'

// Load the persisted app config into memory BEFORE importing the app's
// module graph — composables like useTheme initialize from storage at
// module-evaluation time, so a static `import App` would race the hydrate.
// Tauri reads the Rust backend's ~/.gasket/app_config.json; the browser
// copies localStorage. Never rejects.
initStorage().finally(async () => {
  const { default: App } = await import('./App.vue')
  const app = createApp(App)
  app.use(createPinia())
  app.mount('#app')
})
