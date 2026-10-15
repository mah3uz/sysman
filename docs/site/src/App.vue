<script setup>
import Architecture from './components/Architecture.vue'
import Faq from './components/Faq.vue'
import Features from './components/Features.vue'
import Hardware from './components/Hardware.vue'
import Hero from './components/Hero.vue'
import Install from './components/Install.vue'
import SiteFooter from './components/SiteFooter.vue'
import SiteNav from './components/SiteNav.vue'
import Tour from './components/Tour.vue'
</script>

<template>
  <SiteNav />
  <main id="main">
    <Hero />
    <Features />
    <Tour />
    <Architecture />
    <Hardware />
    <Install />
    <Faq />
  </main>
  <SiteFooter />
</template>
