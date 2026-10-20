import type { Router, SiteData } from 'vitepress'
import type { Ref } from 'vue'

/** localStorage key holding the locale of the last visited page: `root` or a locale key. */
const storageKey = 'touchcue-locale'

function stored(): string | null {
  try {
    return localStorage.getItem(storageKey)
  } catch {
    return null
  }
}

function store(locale: string): void {
  try {
    localStorage.setItem(storageKey, locale)
  } catch {
    // Storage is unavailable, for example when disabled by the browser.
  }
}

/**
 * Locale key of the first language in `languages` the site has. The root
 * locale counts, so a visitor preferring English stays on the English pages.
 */
function preferredLocale(site: SiteData, languages: readonly string[]): string | undefined {
  const locales = Object.entries(site.locales).map(([key, locale]) => ({
    key,
    language: (key === 'root' ? (locale.lang ?? site.lang) : locale.lang)?.split('-')[0].toLowerCase(),
  }))
  for (const language of languages) {
    const wanted = language.split('-')[0].toLowerCase()
    const match = locales.find((locale) => locale.language === wanted)
    if (match) return match.key
  }
  return undefined
}

/** Splits a base-prefixed path into its locale key and the path after the locale prefix. */
function splitLocale(site: SiteData, path: string): { locale: string; rest: string } {
  const relative = path.startsWith(site.base) ? path.slice(site.base.length) : path.replace(/^\//, '')
  const [first, ...rest] = relative.split('/')
  return first !== 'root' && first in site.locales
    ? { locale: first, rest: rest.join('/') }
    : { locale: 'root', rest: relative }
}

/**
 * Browser-only locale handling:
 * - A visitor who opens the site's home page goes to the home page of the
 *   stored locale, or, without a stored locale the site has, of the first
 *   preferred language the site has.
 * - Each route change stores that page's locale, so later visits to the home
 *   page keep the visitor's language.
 */
export function setupLocale(router: Router, siteData: Ref<SiteData>): void {
  const site = siteData.value
  const home = location.pathname.replace(/index(\.html)?$/, '')
  let redirecting = false
  if (home === site.base) {
    const saved = stored()
    const locale =
      saved !== null && Object.keys(site.locales).includes(saved)
        ? saved
        : preferredLocale(site, navigator.languages)
    if (locale && locale !== 'root') {
      redirecting = true
      location.replace(`${site.base}${locale}/${location.search}${location.hash}`)
    }
  }

  const afterRouteChange = router.onAfterRouteChange ?? router.onAfterRouteChanged
  router.onAfterRouteChange = async (to) => {
    await afterRouteChange?.(to)
    const { locale } = splitLocale(site, to)
    if (!redirecting) store(locale)
  }
}
