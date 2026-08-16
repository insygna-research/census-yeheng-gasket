import DOMPurify from 'dompurify';
import hljs from 'highlight.js';
import { marked } from 'marked';

// Maximum characters to parse as markdown; beyond this render as plain text.
const MAX_MARKDOWN_LENGTH = 50000;

export const escapeHtml = (str: string): string =>
  str
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');

const customRenderer = new marked.Renderer();
customRenderer.code = (codeObj: any) => {
  const code = typeof codeObj === 'string' ? codeObj : codeObj.text || '';
  const lang = typeof codeObj === 'string' ? '' : codeObj.lang || '';
  const trimmed = code.trim();
  if (lang === 'mermaid' || trimmed.startsWith('graph ') || trimmed.startsWith('sequenceDiagram') || trimmed.startsWith('classDiagram')) {
    return `<div class="mermaid-diagram my-2 flex justify-center" data-source="${encodeURIComponent(trimmed)}"></div>`;
  }
  const highlighted = hljs.highlightAuto(code).value;
  return `<div class="relative group my-2"><button class="copy-btn absolute top-2 right-2 opacity-0 group-hover:opacity-100 transition-opacity bg-secondary/60 hover:bg-secondary/80 text-secondary-foreground text-[11px] px-2 py-1 rounded backdrop-blur-sm">Copy</button><pre class="hljs rounded-lg p-3 overflow-x-auto text-xs"><code class="language-${lang}">${highlighted}</code></pre></div>`;
};

marked.setOptions({
  breaks: true,
  gfm: true,
  renderer: customRenderer,
});

/**
 * Render message content to sanitized HTML.
 * While streaming, skip marked/DOMPurify (too expensive per chunk) and
 * return escaped text with line breaks. Oversized messages fall back to
 * a plain <pre>.
 */
export function renderMessageContent(content: string, isStreaming: boolean): string {
  if (!content) return '';

  if (isStreaming) {
    return escapeHtml(content).replace(/\n/g, '<br>');
  }

  if (content.length > MAX_MARKDOWN_LENGTH) {
    return `<pre class="whitespace-pre-wrap break-words">${escapeHtml(content)}</pre>`;
  }

  try {
    const raw = marked.parse(content) as string;
    return DOMPurify.sanitize(raw);
  } catch (e) {
    console.error('Markdown parse failed, falling back to plain text:', e);
    return `<pre class="whitespace-pre-wrap break-words">${escapeHtml(content)}</pre>`;
  }
}
