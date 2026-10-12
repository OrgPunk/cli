// Generates crates/convert/src/mime_ext.rs: content type → file extension exactly as SDR's
// static resource conversion picks it (mime 2.6.0 `getExtension`).
// Usage: node scripts/parity/gen-mime-table.mjs > crates/convert/src/mime_ext.rs
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const mime = require('mime');
const types = { ...require('mime/types/standard.js'), ...require('mime/types/other.js') };
const version = require('mime/package.json').version;

const rows = Object.keys(types)
  .map((t) => [t.toLowerCase(), mime.getExtension(t)])
  .filter(([, ext]) => ext)
  .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));

console.log(`//! Content type → extension, generated from mime ${version} (MIT, Robert Kieffer) by
//! scripts/parity/gen-mime-table.mjs. Do not edit by hand.

/// Sorted by content type for binary search.
pub const TABLE: &[(&str, &str)] = &[`);
for (const [t, ext] of rows) console.log(`    (${JSON.stringify(t)}, ${JSON.stringify(ext)}),`);
console.log('];');
