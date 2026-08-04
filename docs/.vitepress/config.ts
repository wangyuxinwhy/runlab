import { defineConfig } from 'vitepress'

// Every page lives under src/runlab/docs so that one tree serves both this
// site and `runlab docs get`, and a relative link between two documents
// resolves the same way in either. Only .vitepress lives here, which keeps
// build output out of the Python package.
export default defineConfig({
  srcDir: '../src/runlab/docs',
  title: 'RunLab',
  description: 'Reproducible Agent execution facts from Base + Overlay + Task',
  lang: 'en-US',
  cleanUrls: true,
  lastUpdated: true,
  markdown: { lineNumbers: false },
  themeConfig: {
    nav: [
      { text: 'Tutorial', link: '/tutorial/your-first-run' },
      { text: 'How-to', link: '/how-to/author-a-base' },
      { text: 'Reference', link: '/reference/model' },
      { text: 'Explanation', link: '/explanation/principles' },
    ],
    sidebar: [
      {
        text: 'Tutorial',
        collapsed: false,
        items: [
          { text: 'Your first Run', link: '/tutorial/your-first-run' },
          { text: 'Your first ablation', link: '/tutorial/your-first-ablation' },
        ],
      },
      {
        text: 'How-to',
        collapsed: false,
        items: [
          { text: 'Author a Base', link: '/how-to/author-a-base' },
          { text: 'Author an Overlay', link: '/how-to/author-an-overlay' },
          { text: 'Supply credentials', link: '/how-to/supply-credentials' },
          { text: 'Recover a lost baseline', link: '/how-to/recover-a-lost-baseline' },
          { text: 'Drive a matrix from a script', link: '/how-to/drive-a-matrix' },
        ],
      },
      {
        text: 'Reference',
        collapsed: false,
        items: [
          { text: 'Model', link: '/reference/model' },
          { text: 'CLI', link: '/reference/cli' },
          { text: 'Architecture', link: '/reference/architecture' },
        ],
      },
      {
        text: 'Explanation',
        collapsed: false,
        items: [
          { text: 'Design principles', link: '/explanation/principles' },
          { text: 'Why the layers exist', link: '/explanation/why-layers' },
          { text: 'Recoverability', link: '/explanation/recoverability' },
        ],
      },
    ],
    outline: { level: [2, 3] },
    socialLinks: [{ icon: 'github', link: 'https://github.com/' }],
    footer: {
      message: 'MIT Licensed',
      copyright: 'RunLab contributors',
    },
    search: { provider: 'local' },
  },
})
