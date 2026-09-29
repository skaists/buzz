import { promises as fs } from "node:fs";
import path from "node:path";

/**
 * Shared "no hand-rolled pubkey truncation" guard.
 *
 * A truncated pubkey prefix is forgeable by vanity-grinding, so display
 * truncation must be consistent and centralized: the canonical
 * `truncatePubkey` in `shared/lib/pubkey.ts` (or the `<PubKey>` component,
 * which also offers full-key reveal + copy). Ad-hoc `pubkey.slice(0, N)`
 * display forms fragmented into five formats before this guard existed.
 *
 * It flags `.slice(` / `.substring(` / `.slice(0` template-truncations applied
 * to identifiers that look like a pubkey/npub, outside the canonical module,
 * and to field names that hold a pubkey without saying so (`owner`, `author`,
 * `signer`, `approver`, `reviewer`, `sender`, `recipient`, `creator`).
 *
 * Known limit: the guard is name-based. A pubkey passed through a generic
 * receiver such as `value` is invisible to it; review is the backstop there.
 * Non-display uses (array windows, color derivation from a key, avatar
 * initials) live in each app's `overrides` allowlist.
 */

const PUBKEY_SLICE_RE =
  /\b[A-Za-z_$][\w$]*(?:[Pp]ubkey|[Pp]ub_key|[Nn]pub)[\w$]*\??\.(?:slice|substring)\(|\b(?:pubkey|pub_key|npub)\??\.(?:slice|substring)\(|\b(?:owner|author|signer|approver|reviewer|sender|recipient|creator)\??\.(?:slice|substring)\(/g;

/**
 * Pure scan of one file's source. Returns every line that hand-rolls a pubkey
 * truncation, with its 1-based line number and trimmed text.
 *
 * @param {string} content
 * @returns {Array<{lineNumber: number, line: string}>}
 */
export function findPubkeyTruncations(content) {
  const hits = [];
  content.split("\n").forEach((line, index) => {
    PUBKEY_SLICE_RE.lastIndex = 0;
    if (PUBKEY_SLICE_RE.test(line)) {
      hits.push({ lineNumber: index + 1, line: line.trim() });
    }
  });
  return hits;
}

async function walkFiles(directory) {
  const entries = await fs.readdir(directory, { withFileTypes: true });
  const files = await Promise.all(
    entries.map(async (entry) => {
      const fullPath = path.join(directory, entry.name);
      if (entry.isDirectory()) {
        return walkFiles(fullPath);
      }
      return [fullPath];
    }),
  );
  return files.flat();
}

/**
 * @param {object} options
 * @param {string} options.projectRoot Absolute path the rule roots resolve against.
 * @param {Array<{root: string, extensions: Set<string>}>} options.rules Where to scan.
 * @param {string} options.label Human label for the failure header.
 * @param {Set<string>} [options.overrides] Allowlisted "relativePath:lineNumber" entries.
 * @param {Set<string>} [options.allowedFiles] Relative paths allowed to truncate (the canonical module).
 * @param {string} options.scriptPath Path mentioned in the failure hint.
 */
export async function runPubkeyTruncationCheck({
  projectRoot,
  rules,
  label,
  overrides = new Set(),
  allowedFiles = new Set(),
  scriptPath,
}) {
  const candidateFiles = (
    await Promise.all(
      rules.map((rule) => {
        const dir = path.join(projectRoot, rule.root);
        return fs
          .access(dir)
          .then(() => walkFiles(dir))
          .catch(() => []);
      }),
    )
  ).flat();

  const violations = [];

  for (const filePath of candidateFiles) {
    const relativePath = path.relative(projectRoot, filePath);
    const rule = rules.find((r) =>
      relativePath.startsWith(`${r.root}${path.sep}`),
    );
    if (!rule || !rule.extensions.has(path.extname(filePath))) {
      continue;
    }
    if (allowedFiles.has(relativePath.split(path.sep).join("/"))) {
      continue;
    }
    if (relativePath.includes(".test.")) {
      continue;
    }

    const content = await fs.readFile(filePath, "utf8");
    for (const hit of findPubkeyTruncations(content)) {
      const key = `${relativePath.split(path.sep).join("/")}:${hit.lineNumber}`;
      if (overrides.has(key)) {
        continue;
      }
      violations.push({ key, line: hit.line });
    }
  }

  if (violations.length > 0) {
    console.error(
      `${label}: found ${violations.length} hand-rolled pubkey truncation(s).\n` +
        `Use \`truncatePubkey\` from shared/lib/pubkey (or the <PubKey> component) instead.\n` +
        `Genuine non-display uses can be allowlisted in ${scriptPath}.\n`,
    );
    for (const violation of violations) {
      console.error(`  ${violation.key}: ${violation.line}`);
    }
    process.exit(1);
  }
}
