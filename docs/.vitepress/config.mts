import { existsSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { type DefaultTheme, defineConfigWithTheme, type LocaleSpecificConfig } from 'vitepress'

/** Which docs a build shows: the latest release tag, or the `main` branch. */
export type Channel = 'latest' | 'next'

export interface ThemeConfig extends DefaultTheme.Config {
  channel: Channel
  /** Text of the banner shown on the `next` channel. */
  versionBanner: { text: string; link: string }
  /** Release tag the `latest` channel is built from; unset when it is built from `main`. */
  latestVersion?: string
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

/** Interface text of one locale. */
interface Strings {
  latest: string
  next: string
  guide: string
  reference: string
  internals: string
  cli: string
  pages: Record<
    | 'getting-started'
    | 'configuration'
    | 'placeholders'
    | 'hooks'
    | 'platforms'
    | 'launchers'
    | 'gpg'
    | 'ssh-askpass'
    | 'architecture',
    string
  >
  versionBanner: ThemeConfig['versionBanner']
  ui: Pick<
    DefaultTheme.Config,
    | 'outline'
    | 'docFooter'
    | 'lastUpdated'
    | 'darkModeSwitchLabel'
    | 'lightModeSwitchTitle'
    | 'darkModeSwitchTitle'
    | 'sidebarMenuLabel'
    | 'returnToTopLabel'
    | 'langMenuLabel'
    | 'skipToContentLabel'
    | 'notFound'
  >
}

const guidePages = [
  'getting-started',
  'configuration',
  'placeholders',
  'hooks',
  'platforms',
  'launchers',
  'gpg',
  'ssh-askpass',
] as const

/** Theme config of the locale whose pages live under `prefix`. */
function themeConfig(prefix: string, strings: Strings): ThemeConfig {
  const latestLabel = latestVersion ? `${strings.latest} (${latestVersion})` : strings.latest
  const nextLabel = `${strings.next} (main)`
  return {
    ...strings.ui,
    channel,
    versionBanner: strings.versionBanner,
    latestVersion,
    nav: [
      { text: strings.guide, link: `${prefix}guide/getting-started` },
      { text: strings.reference, link: `${prefix}reference/cli/` },
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
        text: strings.guide,
        items: guidePages.map((page) => ({
          text: strings.pages[page],
          link: `${prefix}guide/${page}`,
        })),
      },
      {
        text: strings.internals,
        items: [{ text: strings.pages.architecture, link: `${prefix}architecture` }],
      },
      {
        text: strings.reference,
        items: [{ text: strings.cli, link: `${prefix}reference/cli/` }],
      },
    ]),
    socialLinks: [{ icon: 'github', link: 'https://github.com/theaifam5/touchcue' }],
  }
}

const english: Strings = {
  latest: 'Latest',
  next: 'Next',
  guide: 'Guide',
  reference: 'Reference',
  internals: 'Internals',
  cli: 'CLI',
  pages: {
    'getting-started': 'Getting started',
    configuration: 'Configuration',
    placeholders: 'Placeholders',
    hooks: 'Hooks',
    platforms: 'Platforms',
    launchers: 'Launchers',
    gpg: 'gpg and ssh',
    'ssh-askpass': 'ssh askpass',
    architecture: 'Architecture',
  },
  versionBanner: {
    text: 'You are reading the docs for the unreleased main branch.',
    link: 'See the latest release.',
  },
  ui: {},
}

const polish: Strings = {
  latest: 'Najnowsza',
  next: 'Rozwojowa',
  guide: 'Przewodnik',
  reference: 'Dokumentacja',
  internals: 'Budowa wewnętrzna',
  cli: 'CLI',
  pages: {
    'getting-started': 'Pierwsze kroki',
    configuration: 'Konfiguracja',
    placeholders: 'Symbole zastępcze',
    hooks: 'Hooki',
    platforms: 'Platformy',
    launchers: 'Launchery',
    gpg: 'gpg i ssh',
    'ssh-askpass': 'ssh askpass',
    architecture: 'Architektura',
  },
  versionBanner: {
    text: 'Czytasz dokumentację niewydanej gałęzi main.',
    link: 'Zobacz najnowsze wydanie.',
  },
  ui: {
    outline: { label: 'Na tej stronie' },
    docFooter: { prev: 'Poprzednia strona', next: 'Następna strona' },
    lastUpdated: { text: 'Ostatnia aktualizacja' },
    darkModeSwitchLabel: 'Motyw',
    lightModeSwitchTitle: 'Przełącz na jasny motyw',
    darkModeSwitchTitle: 'Przełącz na ciemny motyw',
    sidebarMenuLabel: 'Menu',
    returnToTopLabel: 'Powrót na górę',
    langMenuLabel: 'Zmień język',
    skipToContentLabel: 'Przejdź do treści',
    notFound: {
      title: 'NIE ZNALEZIONO STRONY',
      quote:
        'Ale jeśli nie zmienisz kierunku i będziesz dalej szukać, możesz dotrzeć tam, dokąd zmierzasz.',
      linkLabel: 'przejdź do strony głównej',
      linkText: 'Wróć na stronę główną',
    },
  },
}

const german: Strings = {
  latest: 'Aktuell',
  next: 'Entwicklung',
  guide: 'Anleitung',
  reference: 'Referenz',
  internals: 'Interna',
  cli: 'CLI',
  pages: {
    'getting-started': 'Erste Schritte',
    configuration: 'Konfiguration',
    placeholders: 'Platzhalter',
    hooks: 'Hooks',
    platforms: 'Plattformen',
    launchers: 'Launcher',
    gpg: 'gpg und ssh',
    'ssh-askpass': 'ssh askpass',
    architecture: 'Architektur',
  },
  versionBanner: {
    text: 'Sie lesen die Dokumentation des unveröffentlichten main-Branches.',
    link: 'Zur neuesten Version.',
  },
  ui: {
    outline: { label: 'Auf dieser Seite' },
    docFooter: { prev: 'Vorherige Seite', next: 'Nächste Seite' },
    lastUpdated: { text: 'Zuletzt aktualisiert' },
    darkModeSwitchLabel: 'Erscheinungsbild',
    lightModeSwitchTitle: 'Zum hellen Design wechseln',
    darkModeSwitchTitle: 'Zum dunklen Design wechseln',
    sidebarMenuLabel: 'Menü',
    returnToTopLabel: 'Zurück nach oben',
    langMenuLabel: 'Sprache ändern',
    skipToContentLabel: 'Zum Inhalt springen',
    notFound: {
      title: 'SEITE NICHT GEFUNDEN',
      quote:
        'Aber wenn Sie Ihre Richtung nicht ändern und weitersuchen, kommen Sie vielleicht dort an, wohin Sie unterwegs sind.',
      linkLabel: 'zur Startseite',
      linkText: 'Zur Startseite',
    },
  },
}

const polishDescription = 'Pokazuje, kto czeka na dotknięcie klucza bezpieczeństwa.'
const germanDescription = 'Zeigt, wer auf die Berührung Ihres Sicherheitsschlüssels wartet.'

type Locale = LocaleSpecificConfig<ThemeConfig> & { label: string; link?: string }

// Translations a checkout lacks, such as an older release tag, get no language menu entry.
const translations: [string, Locale][] = [
  [
    'pl',
    {
      label: 'Polski',
      lang: 'pl',
      link: '/pl/',
      description: polishDescription,
      themeConfig: themeConfig('/pl/', polish),
    },
  ],
  [
    'de',
    {
      label: 'Deutsch',
      lang: 'de',
      link: '/de/',
      description: germanDescription,
      themeConfig: themeConfig('/de/', german),
    },
  ],
]

const locales: Record<string, Locale> = {
  root: { label: 'English', lang: 'en', themeConfig: themeConfig('/', english) },
  ...Object.fromEntries(translations.filter(([key]) => existsSync(`${srcDir}${key}/index.md`))),
}

export default defineConfigWithTheme<ThemeConfig>({
  title: 'touchcue',
  description: 'Shows who is waiting for your security key touch.',
  lang: 'en',
  base,
  cleanUrls: true,
  locales,
  themeConfig: locales.root.themeConfig,
})
