import { defineConfig } from 'svedocs/config';
import { hokanOgTemplate } from './og-template.ts';

// svedocs 0.2 uses root-relative routes; deploy this site at an origin root.
const siteUrl = new URL(process.env.SITE_URL || 'https://hokan.pwp.sh');
if (siteUrl.pathname !== '/' || siteUrl.search || siteUrl.hash || !['http:', 'https:'].includes(siteUrl.protocol)) {
  throw new Error('SITE_URL must be an HTTP(S) origin, without a subpath, query, or fragment.');
}

export default defineConfig({
  site: {
    name: 'Hokan',
    title: 'Hokan — shell-aware completion for real terminals',
    description: 'Hokan adds inline completion to zsh, bash, and fish using shell history, command options, files, and project scripts. Installation, guides, and reference.',
    url: siteUrl.origin
  },
  build: {
    mode: 'static'
  },
  theme: {
    defaultMode: 'system',
    readingStyle: 'plain',
    palette: {
      accent: '#285de0',
      neutral: '#202631'
    },
    fonts: {
      sans: 'Inter, ui-sans-serif, system-ui, sans-serif',
      mono: '"JetBrains Mono", "SFMono-Regular", Consolas, monospace',
      display: 'Inter, ui-sans-serif, system-ui, sans-serif'
    },
    radius: '0.625rem',
    codeTheme: {
      light: 'github-light',
      dark: 'github-dark-default'
    },
    code: {
      lineNumbers: false,
      wrap: false,
      copyButton: true
    },
    brand: {
      label: 'hokan',
      href: '/',
      logo: '/logo.svg',
      mark: false
    },
    nav: [
      { label: 'Guides', href: '/docs/getting-started/install' },
      { label: 'Reference', href: '/docs/reference/cli' },
      { label: 'Changelog', href: '/changelog' }
    ],
    social: [
      { label: 'GitHub', href: 'https://github.com/backrunner/hokan', external: true }
    ],
    footer: {
      text: 'Hokan is open source software under the BSD-3-Clause license.',
      links: [
        { label: 'GitHub', href: 'https://github.com/backrunner/hokan', external: true },
        { label: 'License', href: '/license' }
      ]
    },
    home: {
      kicker: 'Shell-aware terminal completion',
      primaryAction: { label: 'Read the docs', href: '/docs' },
      secondaryAction: { label: 'View on GitHub', href: 'https://github.com/backrunner/hokan' }
    }
  },
  search: {
    enabled: true,
    provider: 'local',
    scope: 'current'
  },
  agent: {
    enabled: true,
    markdown: true,
    llms: true,
    negotiation: false
  },
  seo: {
    sitemap: true,
    robots: true,
    rss: false,
    defaultAuthor: 'Hokan contributors',
    defaultAuthorType: 'Organization',
    head: {
      meta: [
        { name: 'theme-color', content: '#111318' }
      ],
      jsonLd: [
        {
          '@context': 'https://schema.org',
          '@type': 'SoftwareApplication',
          name: 'Hokan',
          applicationCategory: 'DeveloperApplication',
          operatingSystem: 'macOS, Linux',
          softwareVersion: '0.1.0-beta.10',
          license: 'https://opensource.org/licenses/BSD-3-Clause',
          url: siteUrl.origin,
          downloadUrl: 'https://github.com/backrunner/hokan/releases',
          offers: { '@type': 'Offer', price: '0', priceCurrency: 'USD' }
        }
      ]
    },
    ogImage: {
      format: 'png',
      renderer: 'satori',
      template: hokanOgTemplate,
      outDir: 'static/og'
    }
  },
  source: {
    editBaseUrl: 'https://github.com/backrunner/hokan/edit/main/website'
  },
  checks: {
    assets: true,
    externalLinks: false,
    translations: false
  },
  i18n: false
});
