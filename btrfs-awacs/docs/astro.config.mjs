// @ts-check

import starlight from '@astrojs/starlight';
import { defineConfig } from 'astro/config';
import mermaid from 'astro-mermaid';
import repositoryLinks from './plugins/repository-links.mjs';

export default defineConfig({
  integrations: [
    repositoryLinks(),
    // The Mermaid integration must install its Markdown transform before
    // Starlight initializes the documentation content collection.
    mermaid({
      autoTheme: true,
      enableLog: false,
      mermaidConfig: {
        flowchart: { curve: 'basis' },
        securityLevel: 'strict',
        startOnLoad: false,
      },
    }),
    starlight({
      title: 'Btrfs AWACS',
      description:
        'Architecture, integration, and correctness review for immutable Btrfs filesystem monitoring.',
      favicon: '/awacs.svg',
      logo: {
        alt: 'Btrfs AWACS',
        src: './src/assets/awacs-mark.svg',
      },
      customCss: ['./src/styles/custom.css'],
      lastUpdated: false,
      tableOfContents: {
        minHeadingLevel: 2,
        maxHeadingLevel: 4,
      },
      sidebar: [
        { label: 'Overview', slug: 'index' },
        {
          label: 'Architecture',
          items: [{ autogenerate: { directory: 'architecture' } }],
        },
        {
          label: 'Lifecycle',
          items: [{ autogenerate: { directory: 'lifecycle' } }],
        },
        {
          label: 'Integrations',
          items: [{ autogenerate: { directory: 'integrations' } }],
        },
        {
          label: 'Operations',
          items: [{ autogenerate: { directory: 'operations' } }],
        },
        {
          label: 'Review findings',
          items: [{ autogenerate: { directory: 'review' } }],
        },
        {
          label: 'Reference',
          items: [{ autogenerate: { directory: 'reference' } }],
        },
      ],
    }),
  ],
});
