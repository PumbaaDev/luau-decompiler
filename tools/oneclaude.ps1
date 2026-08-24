<#
    THE unified terminal. One Claude Code terminal, one endpoint, everything behind it:

      Claude Code  ->  unified_proxy (:8770)  ->  Ollama (:11434 local | cloud)
                          injects memory (916 facts) + a sanitized, uncensored skill
                          routes local <-> cloud per message marker

    You type in one place. Every turn auto-gets the relevant project facts and, when
    the task matches one, a sanitized skill (refusal/disclaimer lines stripped, so the
    abliterated model is never re-censored). Route a single turn to the cloud engine by
    putting @cloud in the message; @local forces local. Default is local (free, offline,
    uncensored).

    This works under `claude --bare` (hooks/plugins off for the 65k window) because the
    proxy injects at the HTTP layer, not via a hook.

    Usage:  .\tools\oneclaude.ps1
            .\tools\oneclaude.ps1 -Default cloud          # default route = cloud
            .\tools\oneclaude.ps1 -Model uncensored-coder-q6
            .\tools\oneclaude.ps1 -NoMcp -p "one-shot prompt"
#>
param(
    [string]$Model   = "uncensored-coder",
    [ValidateSet("local", "cloud")][string]$Default = "local",
    [string]$Effort  = "low",
    [switch]$WithMcp   # curated MCP is OFF by default: a 16384-ctx local model overflows
)                      # on their tool schemas (measured: "hi" alone = 24208 tokens WITH MCP,
                       # over the 16384 window; with no MCP it fits and answers).

$ErrorActionPreference = "Stop"
$repo = Split-Path $PSScriptRoot -Parent
$proxyPort = 8770

# -- 1. Ollama server up. Set the E: model store BEFORE `ollama serve` starts,
#       or it starts against the wrong store and cannot find uncensored-coder.
$env:OLLAMA_MODELS = "E:\ollama-models"
$ollama = "$env:LOCALAPPDATA\Programs\Ollama\ollama.exe"
try {
    Invoke-WebRequest "http://localhost:11434/api/tags" -TimeoutSec 8 -UseBasicParsing | Out-Null
} catch {
    Write-Host "starting ollama server..." -ForegroundColor Yellow
    Start-Process -FilePath $ollama -ArgumentList "serve" -WindowStyle Hidden
    Start-Sleep -Seconds 10
}

# -- 2. Unified proxy up WITH THE RIGHT CONFIG ------------------------------
#       /health reports the live config; if it differs from the requested
#       -Default / -Model, the old proxy is killed and a fresh one started, so
#       the flags are never silently ignored by a stale proxy.
function Start-UnifiedProxy {
    Write-Host "starting unified proxy on :$proxyPort ..." -ForegroundColor Yellow
    $env:OLLAMA_MODELS       = "E:\ollama-models"
    $env:OLLAMA_NUM_PARALLEL = "1"
    $env:PYTHONIOENCODING    = "utf-8"
    $env:UNIFIED_DEFAULT     = $Default
    $env:UNIFIED_LOCAL_MODEL = $Model
    Start-Process -FilePath "python" -ArgumentList "tools\unified_proxy.py" `
        -WorkingDirectory $repo -WindowStyle Hidden
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 500
        try { Invoke-WebRequest "http://localhost:$proxyPort/health" -TimeoutSec 2 -UseBasicParsing | Out-Null; return $true } catch { }
    }
    return $false
}

$needStart = $true
try {
    $cfg = (Invoke-WebRequest "http://localhost:$proxyPort/health" -TimeoutSec 3 -UseBasicParsing).Content | ConvertFrom-Json
    if ($cfg.default -eq $Default -and $cfg.local -eq $Model) {
        $needStart = $false
        Write-Host "reusing proxy on :$proxyPort (default=$($cfg.default) local=$($cfg.local) facts=$($cfg.facts))" -ForegroundColor DarkGray
    } else {
        Write-Host "proxy on :$proxyPort has stale config (default=$($cfg.default) local=$($cfg.local)); restarting" -ForegroundColor Yellow
        $op = (Get-NetTCPConnection -LocalPort $proxyPort -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess
        if ($op) { Stop-Process -Id $op -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 500 }
    }
} catch { }

if ($needStart) {
    if (-not (Start-UnifiedProxy)) { throw "unified proxy did not come up on :$proxyPort (see tools\unified_proxy.log)" }
}

# -- 3. Point Claude Code at the PROXY, not Ollama directly -----------------
$env:ANTHROPIC_BASE_URL         = "http://localhost:$proxyPort"
$env:ANTHROPIC_AUTH_TOKEN       = "ollama"        # dummy; nothing is authenticated
$env:ANTHROPIC_MODEL            = $Model
$env:ANTHROPIC_SMALL_FAST_MODEL = $Model
$env:MAX_THINKING_TOKENS        = "2048"
$env:CLAUDE_CODE_MAX_CONTEXT_TOKENS = "65536"     # local window; else it compacts too late

Remove-Item Env:CLAUDE_CODE_CHILD_SESSION -ErrorAction SilentlyContinue
Remove-Item Env:CLAUDE_CODE_SESSION_ID    -ErrorAction SilentlyContinue

# -- 4. Lean session config (same rationale as local-claude.ps1) ------------
$mcpFile = Join-Path $PSScriptRoot "curated-mcp.json"
if (-not $WithMcp -or -not (Test-Path $mcpFile)) {
    $mcpFile = Join-Path $env:TEMP "empty-mcp.json"
    '{"mcpServers":{}}' | Out-File -FilePath $mcpFile -Encoding ascii -NoNewline
}
$settingsFile = Join-Path $env:TEMP "oneclaude-settings.json"
('{"effortLevel":"' + $Effort + '"}') | Out-File -FilePath $settingsFile -Encoding ascii -NoNewline

$mcpLabel = if ($WithMcp) { "rust,potassium,memory,filesystem" } else { "none (16384 ctx cannot hold MCP schemas)" }
Write-Host ""
Write-Host "  UNIFIED TERMINAL" -ForegroundColor Green
Write-Host "  engines (LOCAL ONLY, both uncensored): @local=$Model (codes)  @think=uncensored-thinker (reasons)" -ForegroundColor Green
Write-Host "  every turn: memory + vault + sanitized skill injected via :$proxyPort; Claude-agent identity stripped" -ForegroundColor DarkGray
Write-Host "  route one turn: put @local or @think in the message (default $Default) - no cloud" -ForegroundColor DarkGray
Write-Host "  ctx=65536  effort=$Effort  mcp=$mcpLabel  (bare: no hooks/plugins)" -ForegroundColor DarkGray
Write-Host ""

& "C:\Users\jep\.local\bin\claude.exe" `
    --bare `
    --strict-mcp-config `
    --mcp-config $mcpFile `
    --settings $settingsFile `
    @args
