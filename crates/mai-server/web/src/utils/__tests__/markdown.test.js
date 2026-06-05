import assert from 'node:assert/strict'

import { renderMarkdown } from '../markdown.js'

assert.equal(
  renderMarkdown('<img src=x onerror=alert(1)>').trim(),
  '&lt;img src=x onerror=alert(1)&gt;'
)

assert.match(renderMarkdown('[docs](https://example.com/docs)'), /href="https:\/\/example\.com\/docs"/)
assert.match(renderMarkdown('[mail](mailto:team@example.com)'), /href="mailto:team@example\.com"/)
assert.match(renderMarkdown('[relative](/settings)'), /href="\/settings"/)
assert.match(renderMarkdown('[doc](docs/page)'), /href="docs\/page"/)
assert.match(renderMarkdown('[anchor](#providers)'), /href="#providers"/)

const scriptLink = renderMarkdown('[boom](javascript:alert(1))')
assert.match(scriptLink, />boom<\/a>|boom/)
assert.doesNotMatch(scriptLink, /href="javascript:/i)

const encodedScriptLink = renderMarkdown('[boom](java\u0000script:alert(1))')
assert.doesNotMatch(encodedScriptLink, /href=/i)

const dataLink = renderMarkdown('[image](data:text/html,<svg onload=alert(1)>)')
assert.doesNotMatch(dataLink, /href="data:/i)

const vbscriptLink = renderMarkdown('[legacy](vbscript:msgbox(1))')
assert.doesNotMatch(vbscriptLink, /href="vbscript:/i)

const protocolRelativeLink = renderMarkdown('[cdn](//example.com/file)')
assert.doesNotMatch(protocolRelativeLink, /href=/i)
