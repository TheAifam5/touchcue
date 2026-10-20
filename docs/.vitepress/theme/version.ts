import { computed, type ComputedRef } from 'vue'
import { useData, useRoute } from 'vitepress'
import type { Channel, ThemeConfig } from '../config.mts'

/** Site path each channel is published under; the docs workflow uses the same paths. */
const roots: Record<Channel, string> = { latest: '/', next: '/next/' }

/**
 * The current page's path in the docs of `channel`. A translated `next` page
 * links to the English home page of a `latest` channel built from a release
 * tag, which may lack the translation.
 */
export function useVersionHref(channel: Channel): ComputedRef<string> {
  const { site, theme, localeIndex } = useData<ThemeConfig>()
  const route = useRoute()
  return computed(() => {
    const unknownTranslation =
      theme.value.channel === 'next' && theme.value.latestVersion && localeIndex.value !== 'root'
    if (channel === 'latest' && unknownTranslation) {
      return roots.latest
    }
    const base = site.value.base
    const page = route.path.startsWith(base) ? route.path.slice(base.length) : ''
    return roots[channel] + page
  })
}
