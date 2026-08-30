import { defineConfig } from '@rspress/core';

export default defineConfig({
  root: 'docs',
  base: '/runlab/',
  title: 'RunLab',
  description:
    'Execute OCI Images and preserve source-traceable Agent Run assets.',
  llms: true,
  themeConfig: {
    llmsUI: {
      injectLlmsHint: true,
      viewOptions: ['markdownLink', 'chatgpt', 'claude'],
    },
  },
});
