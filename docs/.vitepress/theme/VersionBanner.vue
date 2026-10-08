<script setup lang="ts">
import { useData } from 'vitepress'
import { onMounted, onUnmounted, ref } from 'vue'
import type { ThemeConfig } from '../config.mts'
import { useVersionHref } from './version'

const { theme } = useData<ThemeConfig>()
const latest = useVersionHref('latest')
const banner = ref<HTMLElement>()

// The text wraps on narrow screens, so the offset follows the rendered height.
let observer: ResizeObserver | undefined
onMounted(() => {
  if (!banner.value) return
  observer = new ResizeObserver(([entry]) => {
    const height = (entry.target as HTMLElement).offsetHeight
    document.documentElement.style.setProperty('--vp-layout-top-height', `${height}px`)
  })
  observer.observe(banner.value)
})
onUnmounted(() => {
  observer?.disconnect()
  document.documentElement.style.removeProperty('--vp-layout-top-height')
})
</script>

<template>
  <div v-if="theme.channel === 'next'" ref="banner" class="VersionBanner">
    <span>
      You are reading the docs for the unreleased main branch.
      <a :href="latest" target="_self">See the latest release.</a>
    </span>
  </div>
</template>

<style>
/* The default theme offsets the nav, sidebar and content by this height; these
   values cover the first paint, before the banner is measured. */
:root:has(.VersionBanner) {
  --vp-layout-top-height: 40px;
}

@media (max-width: 639px) {
  :root:has(.VersionBanner) {
    --vp-layout-top-height: 56px;
  }
}
</style>

<style scoped>
.VersionBanner {
  position: fixed;
  top: 0;
  z-index: var(--vp-z-index-layout-top);
  display: flex;
  align-items: center;
  justify-content: center;
  width: 100%;
  min-height: 40px;
  padding: 8px 16px;
  font-size: 14px;
  line-height: 20px;
  text-align: center;
  color: var(--vp-c-warning-1);
  /* The soft colour is translucent; the page background keeps scrolled content hidden. */
  background: linear-gradient(var(--vp-c-warning-soft), var(--vp-c-warning-soft)), var(--vp-c-bg);
}

.VersionBanner a {
  margin-left: 0.25em;
  font-weight: 500;
  text-decoration: underline;
}
</style>
