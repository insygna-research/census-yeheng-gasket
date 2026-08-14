import { createApp } from 'vue'
import { createPinia } from 'pinia'
import '@fontsource/inter/400.css'
import '@fontsource/inter/500.css'
import '@fontsource/inter/600.css'
import './styles/index.less'
import App from './App.vue'

const app = createApp(App)
app.use(createPinia())
app.mount('#app')
