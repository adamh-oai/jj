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
      customCss: ['./src/styles/custom.css'],
      lastUpdated: false,
      tableOfContents: {
        minHeadingLevel: 2,
        maxHeadingLevel: 4,
      },
      sidebar: [],
    }),
  ],
});
