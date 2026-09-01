import DOMPurify from "dompurify"
import { marked } from "marked"
import { cva, type VariantProps } from "class-variance-authority"
import { memo, useMemo } from "react"

const markdownVariants = cva("prose min-w-0 max-w-none break-words", {
  variants: {
    variant: {
      default: "text-sm leading-6",
      response: "text-[15px] leading-7 text-foreground [&_h1]:text-2xl [&_h1]:font-semibold [&_h2]:text-xl [&_h2]:font-semibold [&_h3]:text-lg [&_h3]:font-semibold",
      auxiliary: "text-sm leading-6 text-muted-foreground",
    },
  },
  defaultVariants: { variant: "default" },
})

interface MarkdownProps extends VariantProps<typeof markdownVariants> {
  children: string
}

export const Markdown = memo(function Markdown({ children, variant }: MarkdownProps) {
  const html = useMemo(() => DOMPurify.sanitize(marked.parse(children, { async: false }) as string), [children])
  return <div className={markdownVariants({ variant })} dangerouslySetInnerHTML={{ __html: html }} />
})
