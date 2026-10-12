# Security policy

## Reporting a vulnerability

Please report a security problem privately, through GitHub's private vulnerability reporting:
choose [Report a vulnerability](https://github.com/Dray973/Cairn/security/advisories/new) on the
repository's Security page. Please don't open a public issue for it. If the Report a
vulnerability button is missing, open an [issue](https://github.com/Dray973/Cairn/issues/new)
that asks for a private contact, without any details of the problem.

Include what you found, the steps that show it, the Cairn version (About shows it) and the
Windows version and edition. The report is answered privately on GitHub. When a fix is released,
the advisory is published; say in the report whether you want to be credited.

## Supported versions

Only the latest release gets security fixes.

## Scope

Cairn changes Windows settings with administrator rights, so these reports are especially
welcome:

- a way for another program or account to make one of Cairn's elevated processes (the window,
  `optctl.exe` or the scheduled maintenance task) run code, load a library or read or change a
  file it should not;
- a change that Cairn makes without recording it in its journal first, or an Undo that restores
  something other than what was recorded;
- the setup or the uninstaller deleting or changing anything outside Cairn's own folder.

Bugs without a security impact belong in the [issues](https://github.com/Dray973/Cairn/issues).
