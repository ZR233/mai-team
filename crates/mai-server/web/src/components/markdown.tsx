import DOMPurify from "dompurify"
import { marked } from "marked"
import { memo, useMemo } from "react"

export const Markdown = memo(function Markdown({ children }: { children: string }) {
  const html = useMemo(() => DOMPurify.sanitize(marked.parse(children, { async: false }) as string), [children])
  return <div className="prose max-w-none text-sm leading-6" dangerouslySetInnerHTML={{ __html: html }} />
})
