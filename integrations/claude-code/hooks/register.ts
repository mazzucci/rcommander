import type { EngineInterface, Register } from 'claude-code'

import { describeFailure, splitArgs, USAGE } from './util.ts'

type Follow = 'off' | 'goto' | 'diff'

/** Tools whose `file_path` argument names the file they change. */
const EDIT_TOOLS = ['Edit', 'Write', 'MultiEdit'] as const

/** Run `rcmd ctl …`; resolves to an error message, or undefined on success. */
async function ctl(
  $: EngineInterface,
  rcmd: string,
  args: readonly string[],
  init: { stdin?: string; timeoutMs?: number } = {},
): Promise<string | undefined> {
  try {
    const run = await $.process.run([rcmd, 'ctl', '--timeout', '10', ...args], { timeoutMs: 15_000, ...init })
    return run.exitCode === 0 ? undefined : describeFailure(run.stderr || run.stdout)
  } catch {
    return `could not run \`${rcmd}\`; install rcommander or set the plugin's rcmd path`
  }
}

/** Following must never slow Claude down: give up quickly if rcmd is busy or stopped. */
const FOLLOW = { timeoutMs: 3_000 }

export const register: Register = (on, options) => {
  const rcmd = typeof options.rcmd_path === 'string' && options.rcmd_path ? options.rcmd_path : 'rcmd'
  const follow: Follow = options.follow === 'off' || options.follow === 'diff' ? options.follow : 'goto'
  const track = options.track_changes !== false

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
          line: { type: 'integer', description: 'Open the file in the viewer at this line (1-based), highlighted.' },
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
    // `--` keeps paths that start with a dash from being read as options.
    const line = typeof e.line === 'number' && e.line > 0 ? Math.floor(e.line) : undefined
    const args =
      typeof e.compare_with === 'string' && e.compare_with
        ? ['diff', '--', path, e.compare_with]
        : line !== undefined
          ? ['view', '--line', String(line), '--', path]
          : e.view === true
            ? ['view', '--', path]
            : ['goto', '--', path]
    const error = await ctl($, rcmd, args)
    return error ? { deny: `rcommander: ${error}` } : { result: `Shown in rcommander (${args[0]} ${path}${line ? `:${line}` : ''}).` }
  }).catch(() => ({ deny: 'rcommander: showing it failed.' }))

  if (follow === 'off' && !track) return

  for (const tool of EDIT_TOOLS) {
    on('tool.call', { tool }, async ($, e, next) => {
      const file = typeof e.file_path === 'string' ? e.file_path : undefined
      if (!file) return next(e)
      // The file as it was before the edit (empty if it is new). It goes to rcmd on stdin;
      // nothing is written to disk here.
      const before = await $.fs.read(file).then(t => (typeof t === 'string' ? t : ''), () => '')
      const ran = await next(e)
      if (ran.deny === undefined && ran.isError !== true) {
        // rcommander may not be running; all of this is best effort and silent.
        // `track` keeps only the first original of each file, for "Claude's changes".
        if (track) await ctl($, rcmd, ['track', '--', file], { ...FOLLOW, stdin: before })
        if (follow === 'goto') await ctl($, rcmd, ['goto', '--side', 'other', '--', file], FOLLOW)
        if (follow === 'diff') await ctl($, rcmd, ['diff', '--', '-', file], { ...FOLLOW, stdin: before })
      }
      return ran
    })
      // Following is a convenience: whatever goes wrong here, the edit itself goes on
      // (or, if it already ran, its result is replayed).
      .catch(($, e, next) => next(e))
  }
}
