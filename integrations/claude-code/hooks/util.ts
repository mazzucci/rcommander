export const USAGE = [
  'Usage: /rc <command>',
  '  goto <path>        show a directory, or a file in its directory',
  '  diff <a> <b>       side-by-side diff of two files',
  '  view <path>        open a file in the viewer',
  '  select <mask>      select entries matching a mask, e.g. *.rs',
  '  refresh            re-read both panels',
  '  message <text>     show text in the status bar',
  'rcommander (`rcmd` or `rcmd web`) must be running.',
].join('\n')

/** Split slash-command arguments on whitespace, honouring "double" and 'single' quotes. */
export function splitArgs(input: string): string[] {
  const out: string[] = []
  const re = /"([^"]*)"|'([^']*)'|(\S+)/g
  for (const m of input.matchAll(re)) out.push(m[1] ?? m[2] ?? m[3] ?? '')
  return out
}

/** First line of rcmd's error output, without the "Error: " prefix anyhow adds. */
export function describeFailure(output: string): string {
  const line = output.trim().split('\n')[0] ?? ''
  return line.replace(/^Error:\s*/, '') || 'rcmd ctl failed'
}

/** Where the pre-edit copy of `file` goes for the diff view. */
export function snapshotPath(file: string): string {
  const name = file.split('/').pop() || 'file'
  return `/tmp/rcommander-claude/${name}.before`
}
