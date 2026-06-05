import { Renderer, marked } from 'marked'
import hljs from 'highlight.js/lib/common'

const renderer = new Renderer()
renderer.html = ({ text }) => escapeHtml(text)
renderer.link = ({ href, title, tokens }) => {
  const text = marked.parser(tokens || [])
  const safeHref = safeLinkHref(href)
  if (!safeHref) return text
  const titleAttr = title ? ` title="${escapeHtml(title)}"` : ''
  return `<a href="${escapeHtml(safeHref)}"${titleAttr}>${text}</a>`
}

marked.setOptions({ breaks: true, gfm: true, renderer })

export function renderMarkdown(content) {
  if (!content) return ''
  return marked.parse(String(content))
}

export function highlightCodeBlocks(container) {
  if (!container) return
  container.querySelectorAll('pre code').forEach((block) => {
    if (!block.dataset.highlighted) {
      hljs.highlightElement(block)
      block.dataset.highlighted = 'true'
    }
  })
}

function escapeHtml(value) {
  return String(value || '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

function safeLinkHref(value) {
  const href = String(value || '').trim()
  if (!href || /[\u0000-\u001f\u007f]/.test(href)) return null
  const hasExplicitScheme = /^[a-zA-Z][a-zA-Z\d+.-]*:/.test(href)
  try {
    const baseUrl = new URL('https://mai.local/')
    const url = new URL(href, baseUrl)
    if (hasExplicitScheme) {
      return ['http:', 'https:', 'mailto:'].includes(url.protocol) ? href : null
    }
    return url.origin === baseUrl.origin ? href : null
  } catch {
    return null
  }
}
