import { init as initAnalytics } from '@plausible-analytics/tracker'
import { createApp, createSSRApp } from 'vue'
import App from './App.vue'
import { reveal } from './reveal.js'
import './style.css'

initAnalytics({
  domain: 'sysman.asmechanics.com',
  endpoint: 'https://plus.testtlc.com/api/event',
  outboundLinks: true,
})

const root = document.getElementById('app')
// the build pre-renders the page into #app, the dev server leaves it empty
const create = root.firstElementChild ? createSSRApp : createApp

create(App).directive('reveal', reveal).mount(root)
