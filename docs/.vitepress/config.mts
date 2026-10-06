import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'touchcue',
  description: 'Shows who is waiting for your security key touch.',
  base: '/',
  cleanUrls: true,
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Reference', link: '/reference/cli/' },
    ],
    sidebar: [
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
    ],
    socialLinks: [{ icon: 'github', link: 'https://github.com/theaifam5/touchcue' }],
  },
})
