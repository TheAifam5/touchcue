import { computed, type ComputedRef } from 'vue'
import { useData, useRoute } from 'vitepress'
import type { Channel } from '../config.mts'

/** Site path each channel is published under; the docs workflow uses the same paths. */
const roots: Record<Channel, string> = { latest: '/', next: '/next/' }

/** The current page's path in the docs of `channel`. */
export function useVersionHref(channel: Channel): ComputedRef<string> {
  const { site } = useData()
  const route = useRoute()
  return computed(() => {
    const base = site.value.base
    const page = route.path.startsWith(base) ? route.path.slice(base.length) : ''
    return roots[channel] + page
  })
}
