// A Helix tokenizer for syntax highlighting, and the `##` doc-example extractor.
//
// Hand-rolled deliberately: no Shiki/Prism/TextMate grammar exists for Helix, and
// inventing one that DRIFTS from the real lexer would be worse than none. The keyword
// set below is taken from src/lexer.rs; the string forms are the four the lexer
// actually accepts ("…", '…', """…""" raw, '''…''' interpolating).
//
// The extractor mirrors `doc_examples_in` in tests/cli.rs — same `##` + `>>>` rules — so
// the site's "executed in CI" badge means the same thing the gate means. If the two ever
// disagree the badge becomes a lie, which is why both are described by the same doc
// (docs/comments-and-docs.md) and why this file names its counterpart explicitly.

export type TokenKind =
  | "doc"
  | "comment"
  | "string"
  | "interp"
  | "number"
  | "keyword"
  | "builtin"
  | "method"
  | "operator"
  | "punct"
  | "ident"
  | "plain";

export interface Token {
  kind: TokenKind;
  text: string;
}

/** From src/lexer.rs — the complete keyword set. */
const KEYWORDS = new Set([
  "and", "do", "else", "false", "fn", "if", "import", "in", "let", "match",
  "missing", "mut", "not", "or", "then", "true", "try",
]);

/** Globals and common free functions that read as builtins. */
const BUILTINS = new Set([
  "print", "range", "inf", "pi", "e", "dna", "read_int", "read_line", "sqrt",
  "abs", "min", "max", "floor", "ceil", "round", "trunc", "to_int", "to_float",
  "sign", "exp", "log", "sin", "cos", "tan", "len", "is_nan", "dataframe", "tensor",
]);

const OP_CHARS = new Set("+-*/%<>=!&|^~?".split(""));
const PUNCT_CHARS = new Set("()[]{},;:.".split(""));

function isIdentStart(c: string) {
  return /[A-Za-z_]/.test(c);
}
function isIdentPart(c: string) {
  return /[A-Za-z0-9_]/.test(c);
}

/**
 * Tokenize Helix source for display. Never throws: anything unrecognized falls through
 * as `plain`, because a highlighter that can fail is a highlighter that breaks the page.
 */
export function tokenize(src: string): Token[] {
  const out: Token[] = [];
  let i = 0;
  const n = src.length;
  const push = (kind: TokenKind, text: string) => {
    if (text) out.push({ kind, text });
  };

  while (i < n) {
    const c = src[i];

    // Comments: `##` is a doc comment (highlighted distinctly — it is the thing that
    // carries executed examples), `#` an ordinary one.
    if (c === "#") {
      const start = i;
      while (i < n && src[i] !== "\n") i++;
      const text = src.slice(start, i);
      push(text.startsWith("##") ? "doc" : "comment", text);
      continue;
    }

    // Strings. Triples first (longest match), then singles. Interpolation holes are
    // emitted as their own token so they can be tinted inside the string colour.
    if (c === '"' || c === "'") {
      const triple = src.startsWith(c.repeat(3), i);
      const delim = triple ? c.repeat(3) : c;
      // `"""` is RAW in Helix — no interpolation, no escapes.
      const raw = triple && c === '"';
      let j = i + delim.length;
      let lit = delim;
      while (j < n) {
        if (!raw && src[j] === "\\" && j + 1 < n) {
          lit += src.slice(j, j + 2);
          j += 2;
          continue;
        }
        if (src.startsWith(delim, j)) {
          lit += delim;
          j += delim.length;
          break;
        }
        if (!raw && src[j] === "{" && src[j + 1] !== "{") {
          // Flush the literal run, then the hole.
          push("string", lit);
          lit = "";
          let depth = 0;
          const hs = j;
          while (j < n) {
            if (src[j] === "{") depth++;
            else if (src[j] === "}") {
              depth--;
              if (depth === 0) {
                j++;
                break;
              }
            }
            j++;
          }
          push("interp", src.slice(hs, j));
          continue;
        }
        lit += src[j];
        j++;
      }
      push("string", lit);
      i = j;
      continue;
    }

    // Numbers (integer and float; no hex/underscore literals in Helix).
    if (/[0-9]/.test(c)) {
      const start = i;
      while (i < n && /[0-9]/.test(src[i])) i++;
      if (src[i] === "." && /[0-9]/.test(src[i + 1] ?? "")) {
        i++;
        while (i < n && /[0-9]/.test(src[i])) i++;
      }
      if (src[i] === "e" || src[i] === "E") {
        const save = i;
        i++;
        if (src[i] === "+" || src[i] === "-") i++;
        if (/[0-9]/.test(src[i] ?? "")) {
          while (i < n && /[0-9]/.test(src[i])) i++;
        } else {
          i = save;
        }
      }
      push("number", src.slice(start, i));
      continue;
    }

    if (isIdentStart(c)) {
      const start = i;
      while (i < n && isIdentPart(src[i])) i++;
      const word = src.slice(start, i);
      const afterDot = out.length > 0 && out[out.length - 1].text === ".";
      if (KEYWORDS.has(word)) push("keyword", word);
      else if (afterDot) push("method", word);
      else if (BUILTINS.has(word)) push("builtin", word);
      else push("ident", word);
      continue;
    }

    if (OP_CHARS.has(c)) {
      const start = i;
      while (i < n && OP_CHARS.has(src[i])) i++;
      push("operator", src.slice(start, i));
      continue;
    }

    if (PUNCT_CHARS.has(c)) {
      push("punct", c);
      i++;
      continue;
    }

    push("plain", c);
    i++;
  }
  return out;
}

/** One documented example lifted out of a `##` block. */
export interface DocExample {
  /** The `>>>` lines, in order; all but the last are setup. */
  code: string[];
  /** The plain lines beneath — the expected output, compared exactly by the gate. */
  expect: string[];
  /** 1-based line of the first `>>>`, for citing the source file. */
  line: number;
}

/** A `##` documentation block attached to the code that follows it. */
export interface DocBlock {
  /** Prose lines, with the `##` and one leading space stripped. */
  prose: string[];
  examples: DocExample[];
  /** The source lines the block documents, up to the next block or a blank run. */
  code: string;
  line: number;
}

/**
 * Extract `##` doc blocks and their `>>>` examples from a `.helix` file.
 *
 * MIRRORS `doc_examples_in` in tests/cli.rs — consecutive `>>>` lines are one program;
 * the plain lines that follow are its expected output, ending at a blank doc line, the
 * next `>>>`, or the end of the block; the `>>>` indentation is stripped from the
 * expected lines. Keeping the two in step is what makes the site's badge honest.
 */
export function extractDocBlocks(src: string): DocBlock[] {
  const lines = src.split("\n");
  const blocks: DocBlock[] = [];
  let i = 0;

  while (i < lines.length) {
    if (!lines[i].trimStart().startsWith("##")) {
      i++;
      continue;
    }
    const blockStart = i;
    const prose: string[] = [];
    const examples: DocExample[] = [];
    let cur: (DocExample & { indent: number }) | null = null;

    while (i < lines.length && lines[i].trimStart().startsWith("##")) {
      const raw = lines[i].trimStart().slice(2);
      const body = raw.startsWith(" ") ? raw.slice(1) : raw;
      const trimmed = body.trimStart();

      if (trimmed.startsWith(">>>")) {
        const indent = body.length - trimmed.length;
        const code = trimmed.slice(3).trim();
        if (cur && cur.expect.length === 0) {
          cur.code.push(code);
        } else {
          if (cur) examples.push(cur);
          cur = { code: [code], expect: [], line: i + 1, indent };
        }
      } else if (cur) {
        if (trimmed === "") {
          examples.push(cur);
          cur = null;
        } else {
          let line = body.replace(/\s+$/, "");
          for (let k = 0; k < cur.indent; k++) {
            if (line.startsWith(" ")) line = line.slice(1);
          }
          cur.expect.push(line);
        }
      } else {
        prose.push(body.replace(/\s+$/, ""));
      }
      i++;
    }
    if (cur) examples.push(cur);

    // The code this block documents: following lines up to a blank line or the next
    // `##` block (matching how a reader associates a doc comment with its definition).
    const codeLines: string[] = [];
    let j = i;
    while (j < lines.length) {
      const t = lines[j].trimStart();
      if (t.startsWith("##")) break;
      if (t === "" && codeLines.length > 0) break;
      if (t !== "") codeLines.push(lines[j]);
      j++;
    }

    blocks.push({
      prose,
      examples: examples.map(({ code, expect, line }) => ({ code, expect, line })),
      code: codeLines.join("\n"),
      line: blockStart + 1,
    });
  }
  return blocks;
}
