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
          label: 'Repository walkthroughs',
          items: [
            {
              label: '1. Single workspace: serial changes',
              slug: 'walkthroughs/single-workspace-serial',
            },
            {
              label: '2. Single workspace: concurrent changes',
              slug: 'walkthroughs/single-workspace-concurrent',
            },
            {
              label: '3. Initialize a snapshot worktree',
              slug: 'walkthroughs/new-snapshot-worktree',
            },
            {
              label: '4. First changes in the new worktree',
              slug: 'walkthroughs/first-worktree-changes',
            },
          ],
        },
        {
          label: 'Architecture',
          collapsed: true,
          items: [{ autogenerate: { directory: 'architecture' } }],
        },
        {
          label: 'Lifecycle',
          collapsed: true,
          items: [{ autogenerate: { directory: 'lifecycle' } }],
        },
        {
          label: 'Integrations',
          collapsed: true,
          items: [{ autogenerate: { directory: 'integrations' } }],
        },
        {
          label: 'Operations',
          collapsed: true,
          items: [{ autogenerate: { directory: 'operations' } }],
        },
        {
          label: 'Review findings',
          collapsed: true,
          items: [{ autogenerate: { directory: 'review' } }],
        },
        {
          label: 'Reference',
          collapsed: true,
          items: [{ autogenerate: { directory: 'reference' } }],
        },
      ],
    }),
  ],
});
