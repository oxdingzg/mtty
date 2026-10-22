# miao pane Runtime context

`mtty-cli state` accepts `--runtime-context FILE|-` alongside `--session` and
`--cwd`. The JSON value is carried unchanged in the pane's `runtime_context`
state field. Use stdin for local machine paths; never include an administrator
credential, relay token or account password.

An owned Runtime context has `kind: "owned"`, `runtimeID`, and `storage`.
An explicitly attached server has only `kind: "attached"`. JSON `null` clears
the context when the observing client exits. State updates replace the pane's
entry, so missing Session/context fields do not inherit an old client's target.
Different panes retain different contexts even when their Session IDs match.

This is client-reported metadata, not an authorization grant. Desktop Remote
Control must attest the specified local Runtime using miao's private-pipe
bridge, require its observed Runtime ID for mutations, and reject attached,
missing or malformed context. It must never silently select the default local
Runtime. The title-bar account dialog signs the active pane's owned Runtime
into its Hub account through that bridge; other Remote Control windows remain
separate work tracked in #74.
