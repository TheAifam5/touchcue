<script setup lang="ts">
import { useData } from 'vitepress'
import { computed } from 'vue'
import type { Channel, ThemeConfig } from '../config.mts'
import { useVersionHref } from './version'

const props = defineProps<{ channel: Channel; text: string; screenMenu?: boolean }>()
const { theme } = useData<ThemeConfig>()
const href = useVersionHref(props.channel)
const active = computed(() => theme.value.channel === props.channel)
</script>

<!-- `target` makes the router load the page in full: the other channel is a separate site. -->
<template>
  <a
    :class="[screenMenu ? 'screen' : 'menu', { active }]"
    :aria-current="active ? 'page' : undefined"
    :href="href"
    target="_self"
  >{{ text }}</a>
</template>

<style scoped>
.menu {
  display: block;
  border-radius: 6px;
  padding: 0 12px;
  line-height: 32px;
  font-size: 14px;
  font-weight: 500;
  color: var(--vp-c-text-1);
  white-space: nowrap;
  transition: background-color 0.25s, color 0.25s;
}

.menu:hover {
  background-color: var(--vp-c-default-soft);
}

.screen {
  display: block;
  margin-left: 12px;
  line-height: 32px;
  font-size: 14px;
  color: var(--vp-c-text-1);
  transition: color 0.25s;
}

.menu:hover,
.screen:hover,
.active {
  color: var(--vp-c-brand-1);
}
</style>
