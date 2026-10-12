// SDR parity check: converts each corpus project source -> mdapi with SDR (the oracle) and
// with opk's converter, and requires equivalent output. Then SDR's mdapi -> source by both
// must match, and a roundtrip: that source -> mdapi (by opk) must match SDR's mdapi again.
// opk's side is the `parity_convert` example of orgpunk-deploy, the same code a deploy and a pull run.
//
// Usage: node parity.mjs --convert <path to parity_convert> [--corpus <dir>] [--work <dir>] [project...]
//
// Equivalent means the same set of files; XML equal after parsing (whitespace between
// elements ignored, order and comments kept); zipped static resources with the same entries
// and contents; everything else byte for byte. Components opk reports as unsupported are
// counted, not failed: they are explicit errors by design.
// Exit code: 0 all equivalent, 1 differences or failures.

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { ComponentSet, MetadataConverter } from '@salesforce/source-deploy-retrieve';
import { XMLParser } from 'fast-xml-parser';
import JSZip from 'jszip';

const here = path.dirname(fileURLToPath(import.meta.url));
const args = parseArgs(process.argv.slice(2));
const opk = path.resolve(args.convert ?? fail('--convert is required'));
const corpus = path.resolve(args.corpus ?? path.join(here, '../../target/corpus'));
const work = path.resolve(args.work ?? path.join(here, '../../target/parity'));
// Projects without sourceApiVersion: the same version for both sides.
const FALLBACK_API_VERSION = '62.0';

// Directory conversion applies replacements only with this, in SDR and in parity_convert; it
// is what a deploy converts.
process.env.SF_APPLY_REPLACEMENTS_ON_CONVERT = 'true';

const UNSUPPORTED =/is not supported by opk yet|features opk does not support yet/;
const MAX_DIFFS_SHOWN = 20;

function parseArgs(argv) {
  const out = { projects: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) out[a.slice(2)] = argv[++i];
    else out.projects.push(a);
  }
  return out;
}

function fail(msg) {
  console.error(msg);
  process.exit(2);
}

/** All files under dir, relative with `/`, sorted. */
function files(dir) {
  const out = [];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.isFile()) out.push(path.relative(dir, p).split(path.sep).join('/'));
    }
  };
  if (fs.existsSync(dir)) walk(dir);
  return out.sort();
}

async function sdrToMdapi(project, out) {
  const config = JSON.parse(fs.readFileSync(path.join(project, 'sfdx-project.json'), 'utf8'));
  const cs = ComponentSet.fromSource({
    fsPaths: config.packageDirectories.map((p) => path.resolve(project, p.path)),
  });
  cs.sourceApiVersion = config.sourceApiVersion ?? FALLBACK_API_VERSION;
  // Replacements apply only with a project directory, as in `sf project deploy`.
  cs.projectDirectory = project;
  await new MetadataConverter().convert(cs, 'metadata', {
    type: 'directory',
    outputDirectory: out,
    genUniqueDir: false,
  });
}

async function sdrToSource(mdapi, project) {
  const pkg = path.join(project, 'force-app');
  fs.mkdirSync(pkg, { recursive: true });
  const version = fs.readFileSync(path.join(mdapi, 'package.xml'), 'utf8').match(/<version>([^<]+)</)?.[1];
  fs.writeFileSync(
    path.join(project, 'sfdx-project.json'),
    JSON.stringify({ packageDirectories: [{ path: 'force-app', default: true }], sourceApiVersion: version }),
  );
  const cs = ComponentSet.fromSource({ fsPaths: [mdapi] });
  await new MetadataConverter().convert(cs, 'source', {
    type: 'directory',
    outputDirectory: pkg,
    genUniqueDir: false,
  });
}

/** Runs parity_convert mdapi -> source; null on success or the error text. */
function opkToSource(mdapi, out) {
  try {
    execFileSync(opk, ['mdapi', mdapi, out], {
      cwd: work,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return null;
  } catch (e) {
    return (e.stderr?.toString() || e.stdout?.toString() || e.message).trim();
  }
}

/** Runs parity_convert source -> mdapi; null on success or the error text. */
function opkToMdapi(project, out) {
  try {
    execFileSync(opk, ['source', out], {
      cwd: project,
      env: { ...process.env, ORGPUNK_API_VERSION: FALLBACK_API_VERSION },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return null;
  } catch (e) {
    return (e.stderr?.toString() || e.stdout?.toString() || e.message).trim();
  }
}

const parser = new XMLParser({
  preserveOrder: true,
  ignoreAttributes: false,
  ignoreDeclaration: true,
  parseTagValue: false,
  parseAttributeValue: false,
  trimValues: true,
  commentPropName: '#comment',
  processEntities: true,
  htmlEntities: true,
});

const isXml = (buf) => /^\s*</.test(buf.subarray(0, 64).toString('utf8'));
const isZip = (buf) => buf.length > 3 && buf[0] === 0x50 && buf[1] === 0x4b;

/** The first difference between two parsed XML trees, as a path and both values. */
function xmlDiff(a, b, at = '') {
  if (Array.isArray(a) && Array.isArray(b)) {
    const n = Math.max(a.length, b.length);
    for (let i = 0; i < n; i++) {
      if (a[i] === undefined || b[i] === undefined) {
        const extra = a[i] ?? b[i];
        return `${at}: ${a[i] === undefined ? 'opk' : 'SDR'} has extra ${JSON.stringify(extra).slice(0, 200)}`;
      }
      const d = xmlDiff(a[i], b[i], `${at}[${i}]`);
      if (d) return d;
    }
    return null;
  }
  if (a && b && typeof a === 'object' && typeof b === 'object') {
    const keys = [...new Set([...Object.keys(a), ...Object.keys(b)])];
    for (const k of keys) {
      const d = xmlDiff(a[k], b[k], k === ':@' ? `${at}@` : `${at}/${k}`);
      if (d) return d;
    }
    return null;
  }
  return a === b ? null : `${at}: SDR ${JSON.stringify(a)} vs opk ${JSON.stringify(b)}`;
}

async function zipDiff(a, b) {
  const [za, zb] = await Promise.all([JSZip.loadAsync(a), JSZip.loadAsync(b)]);
  const names = (z) => Object.keys(z.files).filter((n) => !z.files[n].dir).sort();
  const [na, nb] = [names(za), names(zb)];
  if (na.join('\n') !== nb.join('\n')) {
    const only = (x, y) => x.filter((n) => !y.includes(n)).slice(0, 5);
    return `zip entries differ: only SDR ${JSON.stringify(only(na, nb))}, only opk ${JSON.stringify(only(nb, na))}`;
  }
  for (const n of na) {
    const [ca, cb] = await Promise.all([za.file(n).async('nodebuffer'), zb.file(n).async('nodebuffer')]);
    if (!ca.equals(cb)) return `zip entry ${n} differs`;
  }
  return null;
}

/** package.xml as `Type:member` strings plus the version; order does not matter. */
function manifestMembers(buf) {
  const xml = buf.toString('utf8');
  const out = new Set();
  for (const [, body] of xml.matchAll(/<types>([\s\S]*?)<\/types>/g)) {
    const name = body.match(/<name>([^<]*)<\/name>/)?.[1].trim();
    for (const [, m] of body.matchAll(/<members>([^<]*)<\/members>/g)) out.add(`${name}:${m.trim()}`);
  }
  out.add(`version ${xml.match(/<version>([^<]*)<\/version>/)?.[1].trim()}`);
  return out;
}

function manifestDiff(a, b) {
  const [ma, mb] = [manifestMembers(a), manifestMembers(b)];
  const onlyA = [...ma].filter((m) => !mb.has(m));
  const onlyB = [...mb].filter((m) => !ma.has(m));
  if (!onlyA.length && !onlyB.length) return null;
  const show = (x) => JSON.stringify(x.slice(0, 10)) + (x.length > 10 ? ` +${x.length - 10}` : '');
  return `members only in SDR ${show(onlyA)}, only in opk ${show(onlyB)}`;
}

/** Differences between SDR's and opk's mdapi directories. */
async function compare(sdrDir, opkDir) {
  const diffs = [];
  const [fa, fb] = [files(sdrDir), files(opkDir)];
  const sb = new Set(fb);
  const sa = new Set(fa);
  for (const f of fa) if (!sb.has(f)) diffs.push(`${f}: only in SDR output`);
  for (const f of fb) if (!sa.has(f)) diffs.push(`${f}: only in opk output`);
  for (const f of fa.filter((x) => sb.has(x))) {
    const [a, b] = [fs.readFileSync(path.join(sdrDir, f)), fs.readFileSync(path.join(opkDir, f))];
    if (a.equals(b)) continue;
    let d;
    if (/(^|\/)(package|destructiveChanges(Pre|Post)?)\.xml$/.test(f)) d = manifestDiff(a, b);
    else if (isZip(a) && isZip(b)) d = await zipDiff(a, b);
    else if (isXml(a) && isXml(b)) {
      try {
        d = xmlDiff(parser.parse(a.toString('utf8')), parser.parse(b.toString('utf8')));
      } catch (e) {
        d = `XML parse error: ${e.message}`;
      }
    } else d = 'content differs';
    if (d) diffs.push(`${f}: ${d}`);
  }
  return diffs;
}

/** Gives every `replaceWithEnv` variable of the project a fixed value both sides see. */
function setReplacementEnv(project) {
  const config = JSON.parse(fs.readFileSync(path.join(project, 'sfdx-project.json'), 'utf8'));
  for (const r of config.replacements ?? []) {
    if (r.replaceWithEnv && process.env[r.replaceWithEnv] === undefined) {
      process.env[r.replaceWithEnv] = `parity-value-of-${r.replaceWithEnv}`;
    }
  }
}

async function check(name, project) {
  setReplacementEnv(project);
  const dir = path.join(work, name);
  fs.rmSync(dir, { recursive: true, force: true });
  const [sdrOut, opkOut] = [path.join(dir, 'sdr'), path.join(dir, 'opk')];
  // Times in ms: SDR's conversion in process, opk's whole run including process start.
  const result = { name, diffs: [], unsupported: null, error: null, sdrMs: 0, opkMs: 0 };

  let t = performance.now();
  try {
    await sdrToMdapi(project, sdrOut);
  } catch (e) {
    result.error = `SDR failed: ${e.message}`;
    return result;
  }
  result.sdrMs = performance.now() - t;
  t = performance.now();
  const err = opkToMdapi(project, opkOut);
  result.opkMs = performance.now() - t;
  if (err) {
    if (UNSUPPORTED.test(err)) result.unsupported = err;
    else result.error = `opk failed: ${err}`;
    return result;
  }
  result.diffs.push(...(await compare(sdrOut, opkOut)));

  // Roundtrip through SDR's source format.
  const rtProject = path.join(dir, 'roundtrip');
  const rtOut = path.join(dir, 'roundtrip-mdapi');
  try {
    await sdrToSource(sdrOut, rtProject);
  } catch (e) {
    result.error = `SDR mdapi -> source failed: ${e.message}`;
    return result;
  }
  // mdapi -> source: opk against SDR, from the same mdapi.
  const srcOut = path.join(dir, 'opk-source');
  const srcErr = opkToSource(sdrOut, srcOut);
  if (srcErr) {
    if (UNSUPPORTED.test(srcErr)) result.unsupported = `mdapi -> source: ${srcErr}`;
    else result.error = `mdapi -> source: opk failed: ${srcErr}`;
    return result;
  }
  result.diffs.push(
    ...(await compare(path.join(rtProject, 'force-app'), srcOut)).map((d) => `mdapi -> source: ${d}`),
  );

  const rtErr = opkToMdapi(rtProject, rtOut);
  if (rtErr) {
    if (UNSUPPORTED.test(rtErr)) result.unsupported = `roundtrip: ${rtErr}`;
    else result.error = `roundtrip: opk failed: ${rtErr}`;
    return result;
  }
  result.diffs.push(...(await compare(sdrOut, rtOut)).map((d) => `roundtrip: ${d}`));
  return result;
}

// Projects as fetch-corpus.sh names them: owner/repo → owner__repo.
const names = args.projects.length
  ? args.projects
  : fs
      .readFileSync(path.join(here, 'corpus.txt'), 'utf8')
      .split('\n')
      .map((l) => l.trim().split(/\s+/)[0])
      .filter((r) => r && !r.startsWith('#'))
      .map((r) => r.replace('/', '__'));
for (const n of names) {
  if (!fs.existsSync(path.join(corpus, n, 'sfdx-project.json'))) {
    fail(`${n}: not in ${corpus}; run fetch-corpus.sh first`);
  }
}

let bad = 0;
const unsupported = [];
const time = { sdr: 0, opk: 0 };
const ms = (x) => `${Math.round(x)} ms`;
for (const name of names) {
  const r = await check(name, path.join(corpus, name));
  if (r.error) {
    bad++;
    console.log(`FAIL ${name}: ${r.error}`);
  } else if (r.unsupported) {
    unsupported.push(r);
    console.log(`SKIP ${name}: ${r.unsupported}`);
  } else if (r.diffs.length) {
    bad++;
    console.log(`DIFF ${name}: ${r.diffs.length} difference(s)`);
    for (const d of r.diffs.slice(0, MAX_DIFFS_SHOWN)) console.log(`  ${d}`);
    if (r.diffs.length > MAX_DIFFS_SHOWN) console.log(`  ... ${r.diffs.length - MAX_DIFFS_SHOWN} more`);
  } else {
    time.sdr += r.sdrMs;
    time.opk += r.opkMs;
    console.log(`OK   ${name} (SDR ${ms(r.sdrMs)}, opk ${ms(r.opkMs)})`);
  }
}
console.log(
  `\n${names.length} projects: ${names.length - bad - unsupported.length} equivalent, ` +
    `${unsupported.length} with unsupported metadata, ${bad} failing`,
);
if (time.opk > 0) {
  console.log(
    `source -> mdapi on equivalent projects: SDR ${ms(time.sdr)}, opk ${ms(time.opk)} ` +
      `(x${(time.sdr / time.opk).toFixed(1)})`,
  );
}
process.exit(bad ? 1 : 0);
