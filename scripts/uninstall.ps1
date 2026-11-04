# Removal is owned by the native installation command.
$ErrorActionPreference = 'Stop'
Write-Error 'Use the installed Lorekeeper 0.120.0 or newer: lore self uninstall. For a custom installation, run that executable with --config PATH self uninstall --data-dir PATH. This script does not remove files; independent deletion cannot validate vault or installation claims.'
exit 1
