import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';
import { svedocs } from 'svedocs/vite';
import svedocsConfig from './svedocs.config.ts';

export default defineConfig({
  plugins: [
    svedocs({
      config: svedocsConfig,
      theme: {
        components: {
          Brand: '$lib/theme/Brand.svelte',
          Sidebar: '$lib/theme/DocumentationNav.svelte'
        }
      }
    }),
    tailwindcss(),
    sveltekit()
  ]
});
