## What you need to do

New installs: follow the install steps above. Existing installs: run `handoff update`. It downloads this release next to the running exe and checks it against the SHA256 in `handoff-version.json`. `handoff update --check` only tells you whether a newer release exists.

If a command fails after updating, run it again with the host console visible. Put the host output for that command in the bug report, along with `%LOCALAPPDATA%\whyknot\handoff\logs\handoff.log`.
