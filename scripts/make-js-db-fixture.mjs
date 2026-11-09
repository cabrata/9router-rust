// Regenerates crates/nr-db/tests/fixtures/js-created-data.sqlite.bin by running
// the ORIGINAL 9router JS database layer (make-js-db-fixture.seed.mjs), so the
// Rust compatibility test reads a file the JS app really produced instead of a
// hand-transcribed DDL string.
//
//   node scripts/make-js-db-fixture.mjs
//
// Requires ../9router (the JS upstream) and node >= 22.5 for its built-in
// node:sqlite driver. Nothing is written outside a temp DATA_DIR.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, copyFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '..');
const jsRoot = path.resolve(repo, '..', '9router');
const seed = path.join(here, 'make-js-db-fixture.seed.mjs');
const out = path.join(repo, 'crates/nr-db/tests/fixtures/js-created-data.sqlite.bin');

// 9router is a Next app: "@/..." aliases and extensionless relative imports do
// not resolve in plain node, so register a resolver hook before importing its
// DB layer. The hook is generated because it embeds this machine's paths.
const dir = mkdtempSync(path.join(tmpdir(), 'jsdb-'));
const hook = path.join(dir, 'loader.mjs');
const register = path.join(dir, 'register.mjs');
writeFileSync(
  hook,
  [
    `const SRC = ${JSON.stringify(path.join(jsRoot, 'src') + '/')};`,
    'export async function resolve(specifier, context, next) {',
    "  const spec = specifier.startsWith('@/') ? SRC + specifier.slice(2) : specifier;",
    '  try { return await next(spec, context); }',
    '  catch (e) {',
    '    const pathLike = spec.startsWith(".") || spec.startsWith("/");',
    '    if (pathLike && e.code === "ERR_MODULE_NOT_FOUND" && !/\\.[cm]?js$/.test(spec)) {',
    '      return next(spec + ".js", context);',
    '    }',
    '    throw e;',
    '  }',
    '}',
    '',
  ].join('\n'),
);
writeFileSync(
  register,
  [
    "import { register } from 'node:module';",
    "import { pathToFileURL } from 'node:url';",
    `register(${JSON.stringify('file://' + hook)}, pathToFileURL(${JSON.stringify(dir + '/')}));`,
    '',
  ].join('\n'),
);

const dataDir = path.join(dir, 'data');
try {
  execFileSync(process.execPath, ['--import', register, seed], {
    cwd: jsRoot,
    stdio: 'inherit',
    env: {
      ...process.env,
      NODE_OPTIONS: '',
      JSDB_9ROUTER_ROOT: jsRoot,
      JSDB_DATA_DIR: dataDir,
    },
  });
  copyFileSync(path.join(dataDir, 'db/data.sqlite'), out);
  console.log('wrote', out);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
