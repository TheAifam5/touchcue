import { inBrowser, type Theme } from 'vitepress'
import DefaultTheme from 'vitepress/theme'
import { h } from 'vue'
import { setupLocale } from './locale'
import VersionBanner from './VersionBanner.vue'
import VersionLink from './VersionLink.vue'

export default {
  extends: DefaultTheme,
  Layout: () => h(DefaultTheme.Layout, null, { 'layout-top': () => h(VersionBanner) }),
  enhanceApp({ app, router, siteData }) {
    // Nav items of the form `{ component: 'VersionLink' }` look the component up by name.
    app.component('VersionLink', VersionLink)
    if (inBrowser) setupLocale(router, siteData)
  },
} satisfies Theme
