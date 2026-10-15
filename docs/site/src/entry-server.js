import { createSSRApp } from 'vue'
import { renderToString } from 'vue/server-renderer'
import App from './App.vue'
import { reveal } from './reveal.js'

export const render = () => renderToString(createSSRApp(App).directive('reveal', reveal))
