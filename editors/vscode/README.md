# zkcl for VS Code

This small extension connects VS Code's debugger to `zkcl dap`. The adapter
uses standard DAP messages and runs a dedicated persistent JSON runtime as a
child process. It is independent of the language server.

Launch an extension development window with
`code --extensionDevelopmentPath=/absolute/path/to/zkcl/editors/vscode`.
Set `zkcl.executablePath` to your built `zkcl` executable if it is not on PATH.

Add this to `.vscode/launch.json` in the project you want to debug:

```json
{
  "version": "0.2.0",
  "configurations": [{
    "type": "zkcl",
    "request": "launch",
    "name": "Debug Lisp",
    "program": "${workspaceFolder}/main.lisp",
    "cwd": "${workspaceFolder}",
    "entry": "(my-package:main)"
  }]
}
```

`program` is loaded after the client finishes setting breakpoints. `entry` is
optional: omit it for a file that calls its own entry point. Debuggable lexical
variables require `(declare (optimize (debug 3)))`. Use the normal call stack,
variables, debug console, pause, continue, step, and restart-frame controls.
The debug console evaluates expressions in the selected live frame and displays
the Lisp printer's result. Watch expressions also run in the live frame and show
compact values for inspection. Expand a result with multiple values to inspect
each value. Hover displays the compact value of a local variable while paused.
References to frames and variables expire when execution resumes. Some native
frames have unavailable lexical variables.

Source breakpoints can be installed before the file is loaded. Conditional
breakpoints accept a Lisp expression. A positive integer hit condition starts
stopping at that hit count. Logpoints substitute Lisp forms in braces, for
example `value={(+ x 1)}`. Restart Frame pauses at the new activation's entry.
The debugger serves the exact compiled source snapshot, including when the
file on disk has since changed.

The debuggee's standard output and error appear in the debug console. DAP does
not provide a standard input prompt, so Lisp reads receive EOF. The extension
does not install a language server, publish packages, or upload source code.
