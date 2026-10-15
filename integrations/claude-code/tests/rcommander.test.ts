import { expect, test } from 'claude-code/testing'
import type { On } from 'claude-code'

/** Stand in for the rcmd binary: record each argv, fail when `failWith` is set. */
function fakeRcmd(on: On, calls: string[][], failWith?: string) {
  on('process.run', async (_$, e) => {
    calls.push([...e.argv])
    return {
      value: {
        exitCode: failWith ? 1 : 0,
        stdout: '',
        stderr: failWith ? `Error: ${failWith}\n` : '',
        isStdoutTruncated: false,
        isStderrTruncated: false,
      },
    }
  })
}

test('/rc passes quoted arguments to rcmd ctl', async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls)
  const out = await $.command.run({ command: 'rc', args: 'goto "/tmp/a b/c.rs"' })
  expect(calls).toEqual([['rcmd', 'ctl', 'goto', '/tmp/a b/c.rs']])
  expect(out.text).toBe('rcommander: goto done.')
})

test('/rc reports rcmd errors and shows usage without arguments', async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls, 'no running rcmd found')
  expect((await $.command.run({ command: 'rc', args: 'refresh' })).text).toBe('rcommander: no running rcmd found')
  expect((await $.command.run({ command: 'rc' })).text).toContain('Usage: /rc')
})

test('the show tool opens a diff or a location', async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls)
  const shown = await $.tool.call({ tool: 'mcp__rcommander__show', path: '/x/a.rs', compare_with: '/y/a.rs' })
  expect(shown.deny).toBeUndefined()
  await $.tool.call({ tool: 'mcp__rcommander__show', path: '/x/a.rs' })
  expect(calls).toEqual([
    ['rcmd', 'ctl', 'diff', '/x/a.rs', '/y/a.rs'],
    ['rcmd', 'ctl', 'goto', '/x/a.rs'],
  ])
})

test('follows edits into the other panel by default', async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls)
  on('tool.call', { tool: 'Edit' }, async () => ({ result: 'edited' }))
  await $.tool.call({ tool: 'Edit', file_path: '/p/main.rs', old_string: 'a', new_string: 'b' })
  expect(calls).toEqual([['rcmd', 'ctl', 'goto', '/p/main.rs', '--side', 'other']])
})

test('diff mode snapshots the file before the edit', { options: { follow: 'diff' } }, async ($, on) => {
  const calls: string[][] = []
  const written: Record<string, string> = {}
  fakeRcmd(on, calls)
  on('fs.read', async () => ({ value: 'old text' }))
  on('fs.write', async (_$, e) => {
    written[e.path] = e.text
    return { value: undefined }
  })
  on('tool.call', { tool: 'Write' }, async () => ({ result: 'written' }))
  await $.tool.call({ tool: 'Write', file_path: '/p/notes.txt', content: 'new text' })
  expect(written).toEqual({ '/tmp/rcommander-claude/notes.txt.before': 'old text' })
  expect(calls).toEqual([['rcmd', 'ctl', 'diff', '/tmp/rcommander-claude/notes.txt.before', '/p/notes.txt']])
})

test('a failed edit is not followed, and follow off does nothing', { options: { follow: 'off' } }, async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls)
  on('tool.call', { tool: 'Edit' }, async () => ({ result: 'edited' }))
  await $.tool.call({ tool: 'Edit', file_path: '/p/main.rs', old_string: 'a', new_string: 'b' })
  expect(calls).toEqual([])
})
