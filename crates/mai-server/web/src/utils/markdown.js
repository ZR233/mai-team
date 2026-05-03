import { Renderer, marked } from 'marked'
import hljs from 'highlight.js/lib/common'

const renderer = new Renderer()
renderer.html = ({ text }) => escapeHtml(text)

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
