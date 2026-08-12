import { fileURLToPath } from 'node:url';

const importedRepositoryDocuments = new Set([
  fileURLToPath(new URL('../../FIXES.md', import.meta.url)),
  fileURLToPath(new URL('../../TODO.md', import.meta.url)),
  fileURLToPath(new URL('../indexed-change-tracking.md', import.meta.url)),
]);

const documentationRoutes = new Map([
  ['FIXES.md', '/reference/current-fixes/'],
  ['TODO.md', '/reference/implementation-roadmap/'],
  ['indexed-change-tracking.md', '/reference/indexed-change-tracking/'],
  ['docs/indexed-change-tracking.md', '/reference/indexed-change-tracking/'],
  ['docs', '/'],
  ['docs/', '/'],
]);

function visibleText(node) {
  if (typeof node.value === 'string') return node.value;
  return (node.children ?? []).map(visibleText).join('');
}

function repositoryMarkdownLinks() {
  return {
    name: 'awacs-repository-markdown-links',

    link(node, context) {
      if (!context.fileURL || !importedRepositoryDocuments.has(fileURLToPath(context.fileURL))) {
        return;
      }

      const destination = node.url;
      if (
        destination.startsWith('/') ||
        destination.startsWith('#') ||
        /^[a-z][a-z\d+.-]*:/i.test(destination)
      ) {
        return;
      }

      const separator = destination.indexOf('#');
      const path = separator < 0 ? destination : destination.slice(0, separator);
      const fragment = separator < 0 ? '' : destination.slice(separator);
      const normalizedPath = path.replace(/^(?:\.\.\/|\.\/)+/, '');

      if (normalizedPath === 'SPEC.md') {
        const route = fragment.includes('verified-implementation-gaps')
          ? '/review/overview/'
          : '/architecture/system-overview/';
        context.replaceNode(node, { ...node, url: route });
        return;
      }

      const route = documentationRoutes.get(normalizedPath);
      if (route) {
        context.replaceNode(node, { ...node, url: `${route}${fragment}` });
        return;
      }

      // Repository source files are intentionally not part of the deployed
      // documentation site. Preserve their rendered label without publishing a
      // misleading link or copying the repository into the static output.
      const replacement =
        node.children?.length === 1
          ? node.children[0]
          : { type: 'text', value: visibleText(node) };
      context.replaceNode(node, replacement);
    },
  };
}

export default function repositoryLinks() {
  return {
    name: 'awacs-repository-links',
    hooks: {
      'astro:config:setup': ({ config }) => {
        const processor = config.markdown.processor;
        if (processor.name !== 'satteri') {
          throw new Error(
            `The AWACS repository-link integration requires the Sätteri Markdown processor, received ${processor.name}.`,
          );
        }

        processor.options.mdastPlugins.push(repositoryMarkdownLinks());
      },
    },
  };
}
