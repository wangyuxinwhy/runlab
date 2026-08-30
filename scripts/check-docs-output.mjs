#!/usr/bin/env node

import { createHash } from 'node:crypto';
import { access, readFile, readdir } from 'node:fs/promises';
import { join, resolve } from 'node:path';

async function listFiles(directory, prefix = '') {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    const relative = join(prefix, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await listFiles(join(directory, entry.name), relative)));
    } else {
      files.push(relative);
    }
  }
  return files;
}

const docsRoot = resolve('docs');
const outputRoot = resolve('doc_build');
const required = [
  'index.html',
  'llms.txt',
  'llms-full.txt',
  'start-here.md',
  'design/generated/run-protocol.md',
  'design/generated/observations.md',
  'reference/cli.md',
  'reference/public-schema.md',
];
for (const relative of required) await access(join(outputRoot, relative));

const sourcePages = (await listFiles(docsRoot))
  .filter(relative => relative.endsWith('.md') || relative.endsWith('.mdx'))
  .map(relative => relative.replace(/\.mdx$/, '.md'));
for (const relative of sourcePages) await access(join(outputRoot, relative));

const snapshot = JSON.parse(
  await readFile(join(docsRoot, 'design', 'generated', '_snapshot.json'), 'utf8'),
);
if (snapshot.schema_version !== 1 || snapshot.pages.length !== 21) {
  throw new Error('design snapshot manifest has an unexpected shape');
}
for (const page of snapshot.pages) {
  const bytes = await readFile(join(docsRoot, 'design', 'generated', page.output));
  const digest = createHash('sha256').update(bytes).digest('hex');
  if (digest !== page.sha256) {
    throw new Error(`generated design page does not match its snapshot: ${page.output}`);
  }
}

const llms = await readFile(join(outputRoot, 'llms.txt'), 'utf8');
for (const expected of [
  'RunLab',
  'Start Here',
  'Run Protocol',
  'Live Event',
  'Observation',
  'CLI Reference',
  'Public SQL Schema',
]) {
  if (!llms.includes(expected)) throw new Error(`llms.txt is missing ${expected}`);
}

const full = await readFile(join(outputRoot, 'llms-full.txt'), 'utf8');
if (Buffer.byteLength(full) > 2 * 1024 * 1024) {
  throw new Error('llms-full.txt exceeds the 2 MiB public documentation budget');
}

const startHtml = await readFile(join(outputRoot, 'start-here.html'), 'utf8');
for (const marker of [
  'rp-llms-hint',
  'Copy Markdown',
  '/runlab/llms.txt',
  '/runlab/llms-full.txt',
  '/runlab/start-here.md',
]) {
  if (!startHtml.includes(marker)) throw new Error(`Start Here is missing ${marker}`);
}

const publicText = await Promise.all(
  (await listFiles(docsRoot))
    .filter(relative => !relative.endsWith('.json'))
    .map(relative => readFile(join(docsRoot, relative), 'utf8')),
);
for (const forbidden of [
  'localhost:8787',
  'code.byted.org',
  '/Users/bytedance',
  'BES/runlab',
]) {
  if (publicText.some(text => text.includes(forbidden))) {
    throw new Error(`public documentation contains forbidden text: ${forbidden}`);
  }
}

process.stdout.write(
  `RSPress Agent-friendly output verified (${sourcePages.length} Markdown pages)\n`,
);
