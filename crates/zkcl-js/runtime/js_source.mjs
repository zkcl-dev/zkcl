// Portable modules use the same layout for compiler metadata and literal data.
const width = 100;
const compactWidth = 64;
const bareKey = /^[A-Za-z_$][A-Za-z0-9_$]*$/u;
const unsafeText = /[\u2028\u2029]|\p{Cf}/gu;

export function jsString(text) {
  return JSON.stringify(text).replace(unsafeText, character => `\\u{${character.codePointAt(0).toString(16)}}`);
}

export function jsIndent(level) { return '  '.repeat(level); }

function stringValue(text, level) {
  const quoted = jsString(text);
  if (quoted.length <= width - level * 2) return quoted;
  const indent = jsIndent(level + 1), available = Math.max(24, width - indent.length - 5);
  const chunks = [];
  let chunk = '', used = 0;
  for (const character of text) {
    const size = jsString(character).length - 2;
    if (used + size > available) {
      chunks.push(jsString(chunk));
      chunk = '';
      used = 0;
    }
    chunk += character;
    used += size;
    if (character === '\n') {
      chunks.push(jsString(chunk));
      chunk = '';
      used = 0;
    }
  }
  if (chunk) chunks.push(jsString(chunk));
  return `(\n${indent}${chunks.join(` +\n${indent}`)}\n${jsIndent(level)})`;
}

function scalarArray(items, level) {
  const indent = jsIndent(level + 1), lines = [];
  let line = indent;
  for (const item of items) {
    if (line.length > indent.length && line.length + item.length + 2 > width) {
      lines.push(line);
      line = indent;
    }
    if (line.length > indent.length) line += ' ';
    line += `${item},`;
  }
  lines.push(line);
  return `[\n${lines.join('\n')}\n${jsIndent(level)}]`;
}

function jsKey(key) {
  if (key === '__proto__') return `[${jsString(key)}]`;
  return bareKey.test(key) && key !== 'package' ? key : jsString(key);
}

export function jsValue(value, level = 0) {
  if (typeof value === 'string') return stringValue(value, level);
  if (value === undefined) return 'null';
  if (value === null || typeof value !== 'object') return JSON.stringify(value);
  const indent = jsIndent(level + 1), close = jsIndent(level);
  if (Array.isArray(value)) {
    if (!value.length) return '[]';
    const items = Array.from(value, item => jsValue(item, level + 1));
    if (value.every(item => item === null || typeof item !== 'object') && items.every(item => !item.includes('\n'))) {
      const compact = `[${items.join(', ')}]`;
      return compact.length <= compactWidth ? compact : scalarArray(items, level);
    }
    return `[\n${items.map(item => `${indent}${item},\n`).join('')}${close}]`;
  }
  const fields = Object.entries(value).filter(([, item]) => item !== undefined);
  if (!fields.length) return '{}';
  const entries = fields.map(([key, item]) => `${jsKey(key)}: ${jsValue(item, level + 1)}`);
  if (fields.every(([, item]) => item === null || typeof item !== 'object') && entries.every(item => !item.includes('\n'))) {
    const compact = `{ ${entries.join(', ')} }`;
    if (compact.length <= compactWidth) return compact;
  }
  return `{\n${entries.map(item => `${indent}${item},\n`).join('')}${close}}`;
}
