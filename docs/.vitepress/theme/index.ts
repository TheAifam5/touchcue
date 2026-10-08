import type { Theme } from 'vitepress'
import DefaultTheme from 'vitepress/theme'
import { h } from 'vue'
import VersionBanner from './VersionBanner.vue'
import VersionLink from './VersionLink.vue'

export default {
  extends: DefaultTheme,
  Layout: () => h(DefaultTheme.Layout, null, { 'layout-top': () => h(VersionBanner) }),
  enhanceApp({ app }) {
    // Nav items of the form `{ component: 'VersionLink' }` look the component up by name.
    app.component('VersionLink', VersionLink)
  },
} satisfies Theme
