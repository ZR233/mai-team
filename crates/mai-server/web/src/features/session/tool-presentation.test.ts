import { describe, expect, it } from "vitest"

import { buildToolPresentation, parseToolText } from "./tool-presentation"

describe("tool presentation model", () => {
  it("presents successful and failed exec results without flattening their output", () => {
    const success = buildToolPresentation({
      name: "exec",
      arguments: JSON.stringify({ command: "cargo test -p mai-server", cwd: "/workspace" }),
      result: JSON.stringify({ status: "completed", exitCode: 0, stdout: "12 tests passed", stderr: "" }),
    })
    const failure = buildToolPresentation({
      name: "exec",
      arguments: JSON.stringify({ command: "cargo test" }),
      result: JSON.stringify({ status: "completed", exitCode: 101, timedOut: true, stdout: "running", stderr: "test failed" }),
    })

    expect(success).toMatchObject({ title: "Run command", summary: "cargo test -p mai-server", status: "completed", failed: false })
    expect(success.sections).toContainEqual({ kind: "code", title: "Standard output", text: "12 tests passed" })
    expect(failure).toMatchObject({ status: "timed out", failed: true })
    expect(failure.sections).toContainEqual({ kind: "code", title: "Standard error", text: "test failed" })

    expect(buildToolPresentation({ name: "exec", status: "budgetLimited", result: '{"status":"completed","exitCode":0}' })).toMatchObject({ status: "budget limited", failed: true })
  })

  it("presents file, patch, note, and GitHub operations with focused fields", () => {
    const read = buildToolPresentation({ name: "read_file", arguments: '{"path":"src/main.rs","startLine":4}', result: '{"path":"src/main.rs","startLine":4,"endLine":8,"text":"fn main() {}"}' })
    const search = buildToolPresentation({ name: "search_files", result: '{"query":"TODO","files":[{"path":"src/lib.rs","matches":[{"line":9,"column":2,"text":"// TODO"}]}],"count":1}' })
    const patch = buildToolPresentation({ name: "apply_patch", arguments: '{"patch":"*** Begin Patch\\n*** Update File: src/lib.rs\\n*** End Patch"}', result: '{"updated":["src/lib.rs"],"changedFiles":["src/lib.rs"]}' })
    const note = buildToolPresentation({ name: "read_session_note", result: '{"revision":7,"totalLines":18,"startLine":6,"endLine":10,"text":"## F-2\\n**Status:** active"}' })
    const github = buildToolPresentation({ name: "github_api_request", arguments: '{"method":"POST","path":"/repos/o/r/pulls/7/reviews","body":{"event":"APPROVE","body":"Looks good"}}', result: '{"id":42,"state":"APPROVED","html_url":"https://github.com/o/r/pull/7#pullrequestreview-42","body":"Looks good","user":{"login":"reviewer"},"repository":{"large":"object"}}' })

    expect(read.summary).toBe("src/main.rs · line 4")
    expect(search.sections).toContainEqual({ kind: "matches", title: "Matches", items: [{ path: "src/lib.rs", line: 9, column: 2, text: "// TODO" }] })
    expect(patch.summary).toBe("src/lib.rs")
    expect(note.sections).toContainEqual({ kind: "markdown", title: "Content", text: "## F-2\n**Status:** active" })
    expect(github.sections).toContainEqual({ kind: "markdown", title: "Review body", text: "Looks good" })
    expect(github.sections.flatMap((section) => section.kind === "fields" ? section.items : [])).not.toContainEqual(expect.objectContaining({ label: "Repository" }))
  })

  it("handles plain, invalid, nested, empty, and unknown results without a JSON fallback", () => {
    expect(parseToolText('"{\\"status\\":\\"ok\\"}"')).toEqual({ value: { status: "ok" }, text: '"{\\"status\\":\\"ok\\"}"', structured: true })

    const plain = buildToolPresentation({ name: "custom_tool", result: "not valid { json" })
    const empty = buildToolPresentation({ name: "custom_tool", result: "[]" })
    const unknown = buildToolPresentation({ name: "custom_tool", result: '{"status":"ready","items":[1,2],"deep":{"private":"value"}}' })

    expect(plain.sections).toEqual([{ kind: "text", title: "Result", text: "not valid { json" }])
    expect(empty.sections).toEqual([{ kind: "text", title: "Result", text: "No items returned." }])
    expect(unknown.sections).toEqual(expect.arrayContaining([
      expect.objectContaining({ kind: "fields", title: "Result" }),
      { kind: "list", title: "Items", items: ["1", "2"] },
    ]))
    expect(unknown.summary).toBe("ready")
  })
})
