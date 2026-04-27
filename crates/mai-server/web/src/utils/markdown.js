import { marked } from 'marked'
import hljs from 'highlight.js'

marked.setOptions({ breaks: true, gfm: true })

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
