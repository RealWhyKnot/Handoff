# Operator Guide (Helper)

This page covers the operator side -- the helper receiving a view URL.

## Opening a Session

The host will send you a URL like:

```
https://handoff.whyknot.dev/v/n1_AbCdEfGhIjK
```

Open it in any modern browser. You get the viewer UI: a command palette on the
left and a results pane on the right. Commands queue immediately when you click
them; results appear as the host bridge processes them.

## Using the CLI Instead

```powershell
.\handoff.exe connect https://handoff.whyknot.dev/v/n1_AbCdEfGhIjK
```

This opens the same viewer in your default browser. You can also pass just the
token (the part after `/v/`):

```powershell
.\handoff.exe connect n1_AbCdEfGhIjK
```

The CLI looks up the relay from `HANDOFF_RELAY` (or uses the default) to
build the full URL.

## Queuing Commands

The viewer lists all available capabilities grouped by category. Each entry
shows the command name, a brief description, and any required arguments. Fill
in arguments where shown and click **Run**. The command is sent to the host
immediately.

Commands with required arguments (such as `net.ping`, which needs a `target`,
or `fs.ls`, which needs a `path`) will show an input before queuing.

Risky commands queue like any other command, but the host must approve the
session warning popup before any risky command runs. A host approval allows
risky commands for the remainder of that session; a host denial blocks them for
the remainder of that session.

For the full list of what each command returns, see [[Capabilities]].

## When the Session Ends

The host presses Ctrl+C to end the session. The viewer shows a disconnected
banner. Results already in the pane remain visible until you close the tab.
