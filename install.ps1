<#
.SYNOPSIS
Build, link, enable, and configure QuotaDeck in one step.

.DESCRIPTION
The Windows counterpart of install.sh. Both scripts must reach the same end
state; when one gains an option, so does the other.

This is the development install, which links a local checkout. A published
install (`herdr plugin install <owner>/<repo>`) never runs this script: Herdr
runs the manifest's [[build]] command and registers the plugin, and the
"Install / repair QuotaDeck" action does the rest. That action is the real
cross-platform installer; this script is the convenience wrapper around it.

Every option is written to the plugin config directory before configure runs.
Herdr executes a plugin action with a fixed command line in the server's own
environment, so variables set around `herdr plugin action invoke` never reach
it; the config directory is the only channel that does.

.PARAMETER Agent
Install only the agents you name: all, claude, codex, grok, agy, opencode, pi,
omp, hermes. Anything left out gets no sidebar row, no statusLine entry and no
hook file. Defaults to every supported agent.

.PARAMETER SidebarLayout
packed (default) joins cache/TTL and 5h/7d on one row. stacked puts each field
on its own row.

.PARAMETER RowGap
1 (default) leaves a blank row between agent panes; 0 packs them flush.

.PARAMETER QuotaPercent
remaining (default) shows how much quota is left; used shows how much has been
consumed. The colour always follows what is left.

.PARAMETER Fields
Quota fields the sidebar shows: all, none, or a comma-separated list of topic,
model, cache, ttl, context, 5h, 7d. The default enables everything except the
prompt-derived topic; use all or name topic explicitly to opt in.

.PARAMETER BrandColors
on (default) tints provider and model with each agent's hue; off leaves them in
the sidebar's own text colour. Severity colours stay either way.

.PARAMETER BrandGlyphs
icon (default) draws each provider's logo from the Herdr Agent Icons Max font.
unicode uses marks that render in any monospace font. off shows names alone —
which is what you want if you already run a plugin that marks agent rows.

.PARAMETER AgentOrder
default leaves Herdr's own agent ordering alone. quota puts the agent with the
least headroom at the top.

.PARAMETER LowQuotaAlert
off (default) never notifies. A percentage notifies once per provider when its
remaining quota falls to that number or below, and again only after recovery.

.PARAMETER OpenRouterKey
Writes an OpenRouter API key into the plugin config directory so the OpenRouter
collector can read it without the key being exported into the Herdr server's
environment. Omit it and the collector falls back to $env:OPENROUTER_API_KEY,
then the active Hermes home's .env.

.PARAMETER HttpProxy
HTTP proxy URL for quota requests (for example http://127.0.0.1:7890), or off
to force direct connections. Stored in the plugin config directory; omitted
values preserve the existing choice. HTTPS requests use an HTTP CONNECT tunnel.

.EXAMPLE
.\install.ps1

.EXAMPLE
.\install.ps1 -Agent claude,codex,hermes -BrandGlyphs off -LowQuotaAlert 10
#>
[CmdletBinding()]
param(
    [string[]] $Agent,
    [int] $WatchIntervalSeconds,
    [ValidateSet('packed', 'stacked')][string] $SidebarLayout,
    [ValidateSet('0', '1')][string] $RowGap,
    [ValidateSet('remaining', 'used')][string] $QuotaPercent,
    [string] $Fields,
    [ValidateSet('on', 'off')][string] $BrandColors,
    [ValidateSet('icon', 'unicode', 'off')][string] $BrandGlyphs,
    [ValidateSet('default', 'quota')][string] $AgentOrder,
    [string] $LowQuotaAlert,
    [string] $OpenRouterKey,
    [string] $HttpProxy
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $root 'scripts\HerdrAction.ps1')

function Die {
    param([string] $Message)
    Write-Error "error: $Message"
    exit 1
}

if (-not (Get-Command herdr -ErrorAction SilentlyContinue)) {
    Die 'Herdr is not installed or not on PATH'
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Die 'Rust/Cargo is not installed or not on PATH'
}

# `0` is accepted as a spelling of off, the same as configure reads it.
if ($LowQuotaAlert) {
    if ($LowQuotaAlert -ne 'off') {
        if ($LowQuotaAlert -notmatch '^\d+$' -or [int]$LowQuotaAlert -gt 100) {
            Die 'LowQuotaAlert must be off or a percentage from 0 to 100'
        }
    }
}
# The field list is validated by configure, which owns the field names.

Write-Host '-> building QuotaDeck'
& cargo build --release --locked --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { Die 'cargo build failed' }
Copy-Item `
    -LiteralPath (Join-Path $root 'target\release\herdr-agent-quota.exe') `
    -Destination (Join-Path $root 'target\release\herdr-agent-quota') `
    -Force `
    -ErrorAction Stop

Write-Host '-> linking and enabling the Herdr plugin'
$linkOutput = & herdr plugin link $root --enabled 2>&1
if ($LASTEXITCODE -ne 0) { Die "herdr plugin link failed: $linkOutput" }

$configDir = (& herdr plugin config-dir $script:HerdrActionPluginId) -join ''
if ($LASTEXITCODE -ne 0 -or -not $configDir) {
    Die 'cannot resolve plugin config directory'
}
$configDir = $configDir.Trim()
New-Item -ItemType Directory -Force -Path $configDir | Out-Null

function Write-PluginPref {
    param([string] $Name, [string] $Value)
    if ([string]::IsNullOrWhiteSpace($Value)) { return }
    # No BOM: the plugin reads these as plain text and a BOM would become part
    # of the value.
    $path = Join-Path $configDir $Name
    [System.IO.File]::WriteAllText($path, "$Value`n", (New-Object System.Text.UTF8Encoding $false))
}

Write-PluginPref 'agents' ($Agent -join ',')
Write-PluginPref 'watch-interval-seconds' $(if ($WatchIntervalSeconds) { "$WatchIntervalSeconds" } else { '' })
Write-PluginPref 'sidebar-layout' $SidebarLayout
Write-PluginPref 'row-gap' $RowGap
Write-PluginPref 'quota-percent' $QuotaPercent
Write-PluginPref 'fields' $Fields
Write-PluginPref 'brand-colors' $BrandColors
Write-PluginPref 'brand-glyphs' $BrandGlyphs
Write-PluginPref 'agent-order' $AgentOrder
Write-PluginPref 'low-quota-alert' $LowQuotaAlert
Write-PluginPref 'http-proxy' $HttpProxy

if ($OpenRouterKey) {
    Write-PluginPref 'openrouter-key' $OpenRouterKey
    Write-Host '-> wrote OpenRouter key to the plugin config directory'
}

Write-Host '-> installing reversible sidebar and provider collectors'
if (-not (Invoke-HerdrActionAndWait -Action 'configure')) {
    Die 'configuration action failed'
}

Write-Host 'Installed. Restart already-running agent sessions once so they load the refreshed hooks.'
Write-Host 'Run `quotadeck setup` any time to see which providers are signed in and how to sign in to the rest.'
