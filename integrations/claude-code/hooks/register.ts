import type { EngineInterface, Register } from 'claude-code'

import { describeFailure, snapshotPath, splitArgs, USAGE } from './util.ts'

type Follow = 'off' | 'goto' | 'diff'

/** Tools whose `file_path` argument names the file they change. */
const EDIT_TOOLS = ['Edit', 'Write', 'MultiEdit'] as const

/** Run `rcmd ctl …`; resolves to an error message, or undefined on success. */
async function ctl($: EngineInterface, rcmd: string, args: readonly string[]): Promise<string | undefined> {
  try {
    const run = await $.process.run([rcmd, 'ctl', ...args], { timeoutMs: 15_000 })
    return run.exitCode === 0 ? undefined : describeFailure(run.stderr || run.stdout)
  } catch {
    return `could not run \`${rcmd}\`; install rcommander or set the plugin's rcmd path`
  }
}

export const register: Register = (on, options) => {
  const rcmd = typeof options.rcmd_path === 'string' && options.rcmd_path ? options.rcmd_path : 'rcmd'
  const follow: Follow = options.follow === 'off' || options.follow === 'diff' ? options.follow : 'goto'

  on('session.start', async ($, e, next) => {
    const started = await next(e)
    await $.command.register({
      name: 'rc',
      description: 'Drive rcommander: goto <path> | diff <a> <b> | view <path> | select <mask> | refresh',
    })
    await $.tool.register({
      name: 'show',
      description:
        "Show the user something in rcommander, their dual-pane file manager (it must already be running). " +
        'Give `path` to point a panel at a directory, or at a file (its directory opens with the cursor on it). ' +
        'Give `path` and `compare_with` to open a side-by-side diff of two files. ' +
        'Use it when the user asks to see files, or to point them at code you are discussing.',
      inputSchema: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'Absolute path of a file or directory.' },
          compare_with: { type: 'string', description: 'Optional second file: opens a diff of path vs this.' },
          view: { type: 'boolean', description: 'Open the file in the viewer instead of the panel.' },
        },
        required: ['path'],
      },
    })
    return started
  })

  on('command.run', { command: 'rc' }, async ($, e) => {
    const args = splitArgs(e.args ?? '')
    if (args.length === 0) return { text: USAGE }
    const error = await ctl($, rcmd, args)
    return { text: error ? `rcommander: ${error}` : `rcommander: ${args[0]} done.` }
  }).catch(() => ({ text: 'rcommander: the command failed.' }))

  on('tool.call', { tool: 'mcp__rcommander__show' }, async ($, e) => {
    const path = typeof e.path === 'string' ? e.path : ''
    if (!path) return { deny: 'path is required' }
    const args =
      typeof e.compare_with === 'string' && e.compare_with
        ? ['diff', path, e.compare_with]
        : e.view === true
          ? ['view', path]
          : ['goto', path]
    const error = await ctl($, rcmd, args)
    return error ? { deny: `rcommander: ${error}` } : { result: `Shown in rcommander (${args[0]} ${args.slice(1).join(' ')}).` }
  }).catch(() => ({ deny: 'rcommander: showing it failed.' }))

  if (follow === 'off') return

  for (const tool of EDIT_TOOLS) {
    on('tool.call', { tool }, async ($, e, next) => {
      const file = e.file_path
      // For a diff, keep the file as it was before the edit (empty if it is new).
      const before = follow === 'diff' ? snapshotPath(file) : undefined
      if (before) {
        const old = await $.fs.read(file).catch(() => '')
        await $.fs.write(before, typeof old === 'string' ? old : '').catch(() => undefined)
      }
      const ran = await next(e)
      if (ran.deny === undefined && ran.isError !== true) {
        // rcommander may not be running; following is best effort and silent.
        await ctl($, rcmd, before ? ['diff', before, file] : ['goto', file, '--side', 'other'])
      }
      return ran
    })
      // Following is a convenience: whatever goes wrong here, the edit itself goes on
      // (or, if it already ran, its result is replayed).
      .catch(($, e, next) => next(e))
  }
}
