import { expect, test } from 'claude-code/testing'
import type { On } from 'claude-code'

/** Stand in for the rcmd binary: record each argv, fail when `failWith` is set. */
/** argv without the leading `rcmd ctl --timeout N`. */
const ctlArgs = (argv: readonly string[]) => argv.slice(4)

function fakeRcmd(on: On, calls: string[][], failWith?: string, stdins: (string | undefined)[] = []) {
  on('process.run', async (_$, e) => {
    calls.push(ctlArgs(e.argv))
    stdins.push(e.init?.stdin)
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
  expect(calls).toEqual([['goto', '/tmp/a b/c.rs']])
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
    ['diff', '--', '/x/a.rs', '/y/a.rs'],
    ['goto', '--', '/x/a.rs'],
  ])
})

test('follows edits into the other panel by default', async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls)
  on('tool.call', { tool: 'Edit' }, async () => ({ result: 'edited' }))
  await $.tool.call({ tool: 'Edit', file_path: '/p/main.rs', old_string: 'a', new_string: 'b' })
  expect(calls).toEqual([['goto', '--side', 'other', '--', '/p/main.rs']])
})

test('diff mode sends the pre-edit content on stdin and writes no file', { options: { follow: 'diff' } }, async ($, on) => {
  const calls: string[][] = []
  const stdins: (string | undefined)[] = []
  let wrote = false
  fakeRcmd(on, calls, undefined, stdins)
  on('fs.read', async () => ({ value: 'old text' }))
  on('fs.write', async () => {
    wrote = true
    return { value: undefined }
  })
  on('tool.call', { tool: 'Write' }, async () => ({ result: 'written' }))
  await $.tool.call({ tool: 'Write', file_path: '/p/notes.txt', content: 'new text' })
  expect(calls).toEqual([['diff', '--', '-', '/p/notes.txt']])
  expect(stdins).toEqual(['old text'])
  expect(wrote).toBe(false)
})

test('a refused or failed edit is not followed', async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls)
  on('tool.call', { tool: 'Edit' }, async () => ({ deny: 'no' }))
  await $.tool.call({ tool: 'Edit', file_path: '/p/main.rs', old_string: 'a', new_string: 'b' })
  expect(calls).toEqual([])
})

test('following never changes the edit result, even when rcmd fails', async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls, 'no running rcmd found')
  on('tool.call', { tool: 'Edit' }, async () => ({ result: 'edited' }))
  const r = await $.tool.call({ tool: 'Edit', file_path: '/p/main.rs', old_string: 'a', new_string: 'b' })
  expect(r.result).toBe('edited')
  expect(calls.length).toBe(1)
})

test('the show tool reports failures and can open the viewer', async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls)
  await $.tool.call({ tool: 'mcp__rcommander__show', path: '/x/a.rs', view: true })
  expect(calls).toEqual([['view', '--', '/x/a.rs']])
  const missing = await $.tool.call({ tool: 'mcp__rcommander__show' })
  expect(missing.deny).toContain('path is required')
})

test('follow off does nothing', { options: { follow: 'off' } }, async ($, on) => {
  const calls: string[][] = []
  fakeRcmd(on, calls)
  on('tool.call', { tool: 'Edit' }, async () => ({ result: 'edited' }))
  await $.tool.call({ tool: 'Edit', file_path: '/p/main.rs', old_string: 'a', new_string: 'b' })
  expect(calls).toEqual([])
})
