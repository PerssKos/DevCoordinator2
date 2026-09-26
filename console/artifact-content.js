'use strict';

window.DevCoordinatorArtifactContent = (() => {
  const UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi;
  const HASH = /\b(?:[0-9a-f]{64}|[0-9a-f]{40}|[0-9a-f]{32})\b/gi;
  const NUMBER = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:e[+-]?\d+)?$/i;
  const WRAPPERS = new Set(['value', 'values', 'data', 'metadata', 'content']);
  const INTERNAL = /^(?:xmlns|namespace|schemalocation|schema|schemaversion|sha\d*|md5|hash|digest|checksum|manifestsha256|treesha256|typedescription)$/i;
  const MAX_NODES = 20000;
  const MAX_DEPTH = 40;

  function cleanText(value) {
    return String(value)
      .replace(/<(script|style)\b[^>]*>[\s\S]*?<\/\1\s*>/gi, '')
      .replace(/<\/?[A-Za-z_][\w:.-]*(?:\s[^<>]*?)?\/?>/g, '')
      .replace(/<!--[\s\S]*?-->|<\?[\s\S]*?\?>/g, '')
      .replace(UUID, '')
      .replace(HASH, (match) => /[a-f]/i.test(match) ? '' : match)
      .replace(/urn:uuid:\s*/gi, '')
      .trim();
  }

  function label(value) {
    const text = cleanText(value).replace(/([a-z\d])([A-Z])/g, '$1 $2').replace(/[_-]+/g, ' ').replace(/\s+/g, ' ').trim();
    return text ? text[0].toUpperCase() + text.slice(1) : '';
  }

  function describe(file) {
    const parts = file.split('/');
    const basename = parts.pop();
    const extension = basename.includes('.') ? basename.split('.').pop().toLowerCase() : '';
    const stem = extension ? basename.slice(0, -extension.length - 1) : basename;
    const title = label(stem) || 'File';
    const folder = parts.map(label).filter(Boolean).join(' / ');
    const kind = /^(png|jpe?g|gif|webp|bmp)$/.test(extension) ? 'Screenshot'
      : extension === 'trx' ? 'Test report'
        : /^(jsonl?|xml|csv|tsv|ya?ml|toml)$/.test(extension) ? 'Data'
          : /^(txt|log|md)$/.test(extension) ? 'Text' : 'File';
    return { title: folder ? `${folder} / ${title}` : title, kind, extension };
  }

  function leaf(name, value, kind) {
    if (INTERNAL.test(name.replace(/[^a-z\d]/gi, ''))) return null;
    const original = String(value);
    if (/^(?:\/(?:home|root|tmp|var\/tmp|run|srv)\/).*?(?:\.devcoordinator|\/scratch\/|\/tmp\/)/.test(original)) return null;
    let text = kind === 'number' ? original : cleanText(original);
    let displayName = name;
    if (!text || /^[\s,;:_./-]*$/.test(text)) return null;
    const valueKind = kind || (NUMBER.test(text) ? 'number' : /^(true|false|null)$/i.test(text) ? 'keyword' : 'string');
    if (valueKind === 'number' && /_nm$/i.test(name) && /^-?\d+$/.test(text)) {
      const digits = text.replace(/^-/, '').padStart(7, '0');
      const whole = digits.slice(0, -6).replace(/^0+(?=\d)/, '');
      const fraction = digits.slice(-6).replace(/0+$/, '');
      text = (text.startsWith('-') ? '-' : '') + whole + (fraction ? '.' + fraction : '');
      displayName = name.slice(0, -3) + ' (mm)';
    } else displayName = name.replace(/_(nm|mm|ms|bytes)$/i, ' ($1)');
    return { name: label(displayName), value: text, kind: valueKind };
  }

  function branch(name, values, array = false) {
    if (INTERNAL.test(name.replace(/[^a-z\d]/gi, ''))) return null;
    const children = values.filter(Boolean).flatMap((child) => !child.name && child.children ? child.children : [child]);
    if (!children.length) return null;
    const wrapper = WRAPPERS.has(name.toLowerCase()) || /\.types\./.test(name);
    if (children.length === 1 && children[0].value != null && /^(value|name|text|content)$/i.test(children[0].name)) {
      return wrapper ? { ...children[0], name: '' } : leaf(name, children[0].value, children[0].kind);
    }
    return { name: wrapper ? '' : label(name), children, array };
  }

  function parseJson(text, budget) {
    JSON.parse(text);
    const tokens = text.match(/"(?:\\.|[^"\\])*"|-?(?:0|[1-9]\d*)(?:\.\d+)?(?:e[+-]?\d+)?|true|false|null|[{}\[\]:,]/gi);
    let cursor = 0;
    const read = (name, depth = 0) => {
      budget(depth);
      const token = tokens[cursor++];
      if (token === '{' || token === '[') {
        const array = token === '[';
        const end = array ? ']' : '}';
        const children = [];
        while (tokens[cursor] !== end) {
          const key = array ? `Item ${children.length + 1}` : JSON.parse(tokens[cursor++]);
          if (!array) cursor += 1;
          children.push(read(key, depth + 1));
          if (tokens[cursor] === ',') cursor += 1;
        }
        cursor += 1;
        return branch(name, children, array);
      }
      return leaf(name, token.startsWith('"') ? JSON.parse(token) : token, token.startsWith('"') ? 'string' : NUMBER.test(token) ? 'number' : 'keyword');
    };
    return read('');
  }

  function parseXml(text, budget) {
    if (/<!DOCTYPE|<!ENTITY/i.test(text)) throw new Error('Unsupported declaration');
    const document = new DOMParser().parseFromString(text, 'application/xml');
    if (document.querySelector('parsererror')) throw new Error('Invalid document');
    const activeDocument = /^(html|svg)$/i.test(document.documentElement.localName);
    const read = (element, depth = 0) => {
      budget(depth);
      if (activeDocument && /^(script|style)$/i.test(element.localName)) return null;
      const attributes = [...element.attributes].filter((attribute) => attribute.prefix !== 'xmlns' && attribute.localName !== 'xmlns')
        .filter((attribute) => element !== document.documentElement || attribute.localName !== 'version')
        .map((attribute) => { budget(depth + 1); return leaf(attribute.localName, attribute.value); });
      const nested = [...element.children].map((child) => read(child, depth + 1));
      const text = [...element.childNodes].filter((child) => child.nodeType === Node.TEXT_NODE || child.nodeType === Node.CDATA_SECTION_NODE).map((child) => child.textContent).join(' ').trim();
      if (!attributes.length && !nested.length) return leaf(element.localName, text);
      return branch(element.localName, [...attributes, ...(text ? [leaf('Text', text)] : []), ...nested]);
    };
    return read(document.documentElement);
  }

  function valueElement(node) {
    const element = document.createElement('span');
    let kind = node.kind;
    if (/^(failed|failure|fatal|error)$/i.test(node.value)) kind = 'failure';
    else if (/^(passed|pass|success|successful|ok)$/i.test(node.value)) kind = 'success';
    else if (/^(warning|warn|skipped|pending)$/i.test(node.value)) kind = 'warning';
    element.className = `log-token log-token-${kind} artifact-value`;
    window.DevCoordinatorI18n.bind(element, () => (node.kind === 'number' ? node.value.replace(/^(-?)(\d{4,})(\.\d+)?$/, (_match, sign, whole, fraction = '') => sign + whole.replace(/\B(?=(\d{3})+(?!\d))/g, ',') + fraction) : node.value));
    return element;
  }

  function renderChildren(children, parent, depth = 0) {
    const priority = (node) => /^(title block|summary|result summary|outcome|status|title|name|test name|message)$/i.test(node.name) ? 0 : 1;
    children = [...children].sort((left, right) => priority(left) - priority(right));
    let fields;
    const counts = new Map();
    for (const child of children) counts.set(child.name, (counts.get(child.name) || 0) + 1);
    const positions = new Map();
    for (const child of children) {
      positions.set(child.name, (positions.get(child.name) || 0) + 1);
      const title = `${child.name || 'Value'}${counts.get(child.name) > 1 ? ` ${positions.get(child.name)}` : ''}`;
      if (child.value != null) {
        if (!fields) {
          fields = document.createElement('dl');
          fields.className = 'artifact-fields';
          parent.append(fields);
        }
        const row = document.createElement('div');
        row.className = 'artifact-field';
        const term = document.createElement('dt');
        term.className = 'log-token log-token-key';
        term.textContent = title;
        const value = document.createElement('dd');
        value.append(valueElement(child));
        row.append(term, value);
        fields.append(row);
        continue;
      }
      fields = null;
      const group = document.createElement('details');
      group.className = 'artifact-data-group';
      const summary = document.createElement('summary');
      const heading = document.createElement('span');
      heading.textContent = title;
      const count = document.createElement('span');
      count.className = 'artifact-group-count';
      count.textContent = `${child.children.length} ${child.array ? 'items' : 'fields'}`;
      summary.append(heading, count);
      group.append(summary);
      let rendered = false;
      const reveal = () => {
        if (rendered) return;
        rendered = true;
        const body = document.createElement('div');
        body.className = 'artifact-group-body';
        renderChildren(child.children, body, depth + 1);
        group.append(body);
      };
      summary.addEventListener('click', () => { if (!group.open) reveal(); });
      group.addEventListener('toggle', () => { if (group.open) reveal(); });
      const measurements = child.children.length <= 4 && child.children.every((entry) => entry.kind === 'number' && /\((mm|nm|ms|bytes)\)$/.test(entry.name));
      group.open = !child.array && ((depth === 0 && child.children.some((entry) => entry.value != null)) || (depth < 2 && measurements));
      if (group.open) reveal();
      parent.append(group);
    }
  }

  function render(text, file, { truncated = false, highlight } = {}) {
    const container = document.createElement('div');
    container.className = 'artifact-data';
    const extension = describe(file).extension;
    const trimmed = text.trim();
    const isJson = extension === 'json' || /^[{[]/.test(trimmed);
    const isXml = /^(xml|xsd|trx|svg|html)$/.test(extension) || /^<(?:\?xml|[A-Za-z_][\w:.-]*[\s>])/.test(trimmed);
    let count = 0;
    const budget = (depth) => {
      if (++count > MAX_NODES || depth > MAX_DEPTH) throw new Error('Preview complexity limit');
    };
    try {
      let root;
      if (extension === 'jsonl') root = branch('', trimmed.split(/\r?\n/).filter((line) => line.trim()).map((line, index) => {
        const item = parseJson(line, budget);
        return item ? { ...item, name: `Entry ${index + 1}` } : null;
      }), true);
      else if (isJson) root = parseJson(trimmed, budget);
      else if (isXml) root = parseXml(trimmed, budget);
      else {
        const visible = cleanText(text);
        if (visible) {
          const pre = document.createElement('pre');
          pre.className = 'artifact-plain-text';
          if (highlight) pre.innerHTML = highlight(visible);
          else pre.textContent = visible;
          container.append(pre);
          return container;
        }
      }
      if (root) renderChildren(root.children || [root], container);
      if (!container.childElementCount) window.DevCoordinatorI18n.text(container, "artifacts.no_readable_data_in_this_file_the_original_is_av_7a8394");
    } catch {
      container.classList.add('artifact-data-unavailable');
      window.DevCoordinatorI18n.bind(container, () => (truncated ? window.DevCoordinatorI18n.t("artifacts.download_file_to_read_the_complete_data_6918f5") : window.DevCoordinatorI18n.t("artifacts.this_file_could_not_be_shown_as_readable_data_th_21c7d8")));
    }
    return container;
  }

  return { describe, render };
})();
