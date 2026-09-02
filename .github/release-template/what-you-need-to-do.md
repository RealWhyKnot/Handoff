## What you need to do

Fresh install: follow the install steps above. Existing installs: run `handoff update`, which downloads this release next to the running exe and checks it against the SHA256 in `handoff-version.json`. `handoff update --check` only reports whether a newer release exists.

If a command misbehaves after this release: rerun it with the host console visible, copy the host output from the failed command, and include it in the bug report together with `%LOCALAPPDATA%\whyknot\handoff\logs\handoff.log`.
