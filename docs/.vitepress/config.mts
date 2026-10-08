import { existsSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { type DefaultTheme, defineConfigWithTheme } from 'vitepress'

/** Which docs a build shows: the latest release tag, or the `main` branch. */
export type Channel = 'latest' | 'next'

export interface ThemeConfig extends DefaultTheme.Config {
  channel: Channel
}

// The docs workflow sets these; without them the build is the `main` docs at `/`.
const channel = process.env.DOCS_CHANNEL ?? 'next'
if (channel !== 'latest' && channel !== 'next') {
  throw new Error(`DOCS_CHANNEL must be "latest" or "next", not "${channel}"`)
}
const base = process.env.DOCS_BASE ?? '/'
if (!base.startsWith('/') || !base.endsWith('/')) {
  throw new Error(`DOCS_BASE must start and end with "/", not "${base}"`)
}
const latestVersion = process.env.DOCS_LATEST_VERSION
if (latestVersion && !/^v[0-9]+\.[0-9]+\.[0-9]+$/.test(latestVersion)) {
  throw new Error(`DOCS_LATEST_VERSION must be a release tag like v1.2.3, not "${latestVersion}"`)
}
const latestLabel = latestVersion ? `Latest (${latestVersion})` : 'Latest'
const nextLabel = 'Next (main)'

const srcDir = fileURLToPath(new URL('..', import.meta.url))

// Drops sidebar links to pages this checkout lacks, and groups left empty, so
// an older release tag built with this config gets no links to later pages.
function present(groups: DefaultTheme.SidebarItem[]): DefaultTheme.SidebarItem[] {
  return groups
    .map((group) => ({
      ...group,
      items: group.items?.filter(({ link }) => {
        if (!link) return true
        const page = link.endsWith('/') ? `${link}index.md` : `${link}.md`
        return existsSync(`${srcDir}${page}`)
      }),
    }))
    .filter(({ items }) => items?.length)
}

export default defineConfigWithTheme<ThemeConfig>({
  title: 'touchcue',
  description: 'Shows who is waiting for your security key touch.',
  base,
  cleanUrls: true,
  themeConfig: {
    channel,
    nav: [
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Reference', link: '/reference/cli/' },
      {
        text: channel === 'latest' ? latestLabel : nextLabel,
        items: [
          { component: 'VersionLink', props: { channel: 'latest', text: latestLabel } },
          { component: 'VersionLink', props: { channel: 'next', text: nextLabel } },
        ],
      },
    ],
    sidebar: present([
      {
        text: 'Guide',
        items: [
          { text: 'Getting started', link: '/guide/getting-started' },
          { text: 'Configuration', link: '/guide/configuration' },
          { text: 'Placeholders', link: '/guide/placeholders' },
          { text: 'Hooks', link: '/guide/hooks' },
          { text: 'Platforms', link: '/guide/platforms' },
          { text: 'Launchers', link: '/guide/launchers' },
          { text: 'gpg and ssh', link: '/guide/gpg' },
          { text: 'ssh askpass', link: '/guide/ssh-askpass' },
        ],
      },
      {
        text: 'Internals',
        items: [{ text: 'Architecture', link: '/architecture' }],
      },
      {
        text: 'Reference',
        items: [{ text: 'CLI', link: '/reference/cli/' }],
      },
    ]),
    socialLinks: [{ icon: 'github', link: 'https://github.com/theaifam5/touchcue' }],
  },
})
