# Usage: powershell -File headsup-send.ps1 <stop|notification|stopfailure|question|permission|test-notify|test-logo|reload-config|quit>
# Forwards the Claude Code hook JSON from stdin to the overlay; starts the overlay if needed.
param([string]$Type = 'stop')

$ErrorActionPreference = 'SilentlyContinue'
try { [Console]::InputEncoding = [System.Text.Encoding]::UTF8 } catch {}
# Send the body at once instead of waiting for a "100 Continue".
try { [System.Net.ServicePointManager]::Expect100Continue = $false } catch {}
$Type = ($Type -replace '[^a-z-]', '')
# Not %APPDATA%: the Claude app (an MSIX package) redirects AppData for the processes it starts.
$appDir = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.headsup'
$cfg = $null
# ReadAllText decodes UTF-8 with or without a BOM (Get-Content would use the ANSI code page).
try { $cfg = [System.IO.File]::ReadAllText((Join-Path $appDir 'config.json')) | ConvertFrom-Json } catch {}
$port = 47821
if ($cfg -and $cfg.port) { $port = [int]$cfg.port }

$raw = ''
try { if ([Console]::IsInputRedirected) { $raw = [Console]::In.ReadToEnd() } } catch {}
if (-not $raw -or -not $raw.Trim()) { $raw = '{}' }
# StopFailure: who runs the session (cli, claude-desktop, sdk-cli) is only in the environment;
# it goes in as "headsup_entrypoint" (letters, digits and - only, so it never needs escaping).
if ($Type -eq 'stopfailure') {
    $ep = ([string]$env:CLAUDE_CODE_ENTRYPOINT) -replace '[^A-Za-z0-9-]', ''
    $t = $raw.Trim()
    if ($ep -and $t.StartsWith('{')) {
        $rest = $t.Substring(1).TrimStart()
        $sep = ','
        if ($rest.StartsWith('}')) { $sep = '' }
        $raw = '{"headsup_entrypoint":"' + $ep + '"' + $sep + $rest
    }
}
$bodyBytes = [System.Text.Encoding]::UTF8.GetBytes($raw)

# Interactive types (question, permission) block until the user clicks a button on the
# overlay or the wait expires; the overlay's reply is printed as the hook's JSON output.
$interactive = ($Type -eq 'question' -or $Type -eq 'permission')
$waitMs = 20000
if ($cfg -and $cfg.interactive -and $cfg.interactive.waitMs) { $waitMs = [int]$cfg.interactive.waitMs }
$holdMs = 480000
if ($cfg -and $cfg.phone -and $cfg.phone.holdMs) { $holdMs = [int]$cfg.phone.holdMs }
if ($holdMs -gt $waitMs) { $waitMs = $holdMs }

# ---- Requests are signed with the per-install key; replies must carry the overlay's
# signature, so nothing else that took the port can answer a permission request.
function Get-Key {
    try {
        $k = [System.IO.File]::ReadAllText((Join-Path $appDir 'secret.key')).Trim()
        if ($k -cmatch '^[0-9a-f]{64}$') { return $k }
    } catch {}
    return $null
}

function Get-Hex([byte[]]$b) { return [BitConverter]::ToString($b).Replace('-', '').ToLowerInvariant() }

function Get-Mac([string]$key, [string]$prefix, [byte[]]$data) {
    $h = New-Object System.Security.Cryptography.HMACSHA256 (, [System.Text.Encoding]::UTF8.GetBytes($key))
    $head = [System.Text.Encoding]::UTF8.GetBytes($prefix)
    $msg = New-Object byte[] ($head.Length + $data.Length)
    [Array]::Copy($head, 0, $msg, 0, $head.Length)
    [Array]::Copy($data, 0, $msg, $head.Length, $data.Length)
    return Get-Hex ($h.ComputeHash($msg))
}

function Test-Listening {
    try {
        foreach ($l in [System.Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()) {
            if ($l.Port -eq $port) { return $true }
        }
        return $false
    } catch { return $true }
}

# One signed request (a fresh nonce every time: the overlay refuses a nonce it has seen).
# Returns $true when our overlay answered; $false when something else answered or the key
# was refused; throws when nothing answered in time.
function Send-Event([string]$key, [int]$timeoutMs) {
    $nb = New-Object byte[] 16
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($nb)
    $nonce = Get-Hex $nb
    $req = [System.Net.WebRequest]::Create("http://127.0.0.1:$port/$Type")
    $req.Method = 'POST'
    $req.ContentType = 'application/json; charset=utf-8'
    $req.Timeout = $timeoutMs
    $req.ReadWriteTimeout = $timeoutMs
    $req.Proxy = $null
    $req.Headers.Add('X-Overlay-Nonce', $nonce)
    $req.Headers.Add('X-Overlay-Auth', (Get-Mac $key "req`n$Type`n$nonce`n" $bodyBytes))
    # The overlay closes this request's card when this process is gone (Esc in Claude Code).
    $req.Headers.Add('X-Hook-Pid', [string]$PID)
    $req.ContentLength = $bodyBytes.Length
    $stream = $req.GetRequestStream()
    $stream.Write($bodyBytes, 0, $bodyBytes.Length)
    $stream.Close()
    try { $resp = $req.GetResponse() }
    catch {
        $ex = $_.Exception
        while ($ex -and -not ($ex -is [System.Net.WebException])) { $ex = $ex.InnerException }
        if ($ex -and $ex.Response) { try { $ex.Response.Close() } catch {}; return $false }
        throw
    }
    $ms = New-Object System.IO.MemoryStream
    $rs = $resp.GetResponseStream()
    $rs.CopyTo($ms)
    $rs.Close()
    $sig = [string]$resp.Headers['X-Overlay-Sig']
    $resp.Close()
    $reply = $ms.ToArray()
    if (-not $sig -or $sig.Trim().ToLowerInvariant() -ne (Get-Mac $key "resp`n$nonce`n" $reply)) { return $false }
    $body = [System.Text.Encoding]::UTF8.GetString($reply)
    if ($interactive -and $body.Trim().StartsWith('{') -and $body.Contains('hookSpecificOutput')) {
        [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
        [Console]::Out.Write($body)
        [Console]::Out.Flush()
    }
    return $true
}

# The key appears right after the overlay binds the port: wait for it briefly.
function Wait-Key([int]$ms) {
    $until = (Get-Date).AddMilliseconds($ms)
    while ($true) {
        $k = Get-Key
        if ($k -or (Get-Date) -gt $until) { return $k }
        Start-Sleep -Milliseconds 100
    }
}

# Starts the overlay unless the user quit it from the tray or removed the hooks, this
# process runs elevated (the overlay would inherit Administrator rights), or a start failed
# a moment ago. $true when it is starting (here or from another hook just now).
function Start-Overlay {
    $exe = $null
    if ($cfg) { $exe = [string]$cfg.overlayExe }
    if (-not $exe -or -not (Test-Path -LiteralPath $exe -PathType Leaf)) { return $false }
    if ((Test-Path -LiteralPath (Join-Path $appDir 'stopped')) -or (Test-Path -LiteralPath (Join-Path $appDir 'uninstalled'))) { return $false }
    try {
        $me = New-Object System.Security.Principal.WindowsPrincipal ([System.Security.Principal.WindowsIdentity]::GetCurrent())
        if ($me.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)) { return $false }
    } catch {}
    try { if (((Get-Date) - (Get-Item -LiteralPath $failStamp).LastWriteTime).TotalSeconds -lt 300) { return $false } } catch {}
    try { if (((Get-Date) - (Get-Item -LiteralPath $spawnStamp).LastWriteTime).TotalSeconds -lt 10) { return $true } } catch {}
    try { Set-Content -LiteralPath $spawnStamp -Value (Get-Date).Ticks } catch {}
    $argList = @()
    if ($cfg.overlayArgs) { $argList = @($cfg.overlayArgs | ForEach-Object { '"' + $_ + '"' }) }
    $argList += '--from-hook'
    # Its own folder as the working directory: never hold the project folder open.
    try { Start-Process -FilePath $exe -ArgumentList $argList -WorkingDirectory (Split-Path -Parent $exe) -WindowStyle Hidden -ErrorAction Stop; return $true } catch {}
    try { Start-Process -FilePath $exe -ArgumentList $argList -WindowStyle Hidden -ErrorAction Stop; return $true } catch { return $false }
}

$spawnStamp = Join-Path $env:TEMP 'headsup-spawn.stamp'
$failStamp = Join-Path $env:TEMP 'headsup-spawn.failed'
# Non-interactive events are fire-and-forget; interactive ones may be extended by the
# overlay while an answer is typed.
$timeout = 2500
if ($interactive) { $timeout = $waitMs + 190000 }

if (-not (Test-Listening)) {
    # Overlay not running (checked without a connect, which takes ~1 s on a closed port).
    if ($Type -eq 'quit') { exit 0 }
    if (-not (Start-Overlay)) { exit 0 }
    $startWait = 3000
    if ($interactive) { $startWait = 6000 }
    $until = (Get-Date).AddMilliseconds($startWait)
    while (-not (Test-Listening)) {
        if ((Get-Date) -gt $until) {
            # It did not come up: do not try again (and wait again) for a while.
            try { Set-Content -LiteralPath $failStamp -Value (Get-Date).Ticks } catch {}
            exit 0
        }
        Start-Sleep -Milliseconds 100
    }
}
$key = Wait-Key 2000
if (-not $key) { exit 0 }
# A single attempt: a request that timed out may still have been shown, and a retry would
# show it twice.
try { [void](Send-Event $key $timeout) } catch {}
exit 0
