<#
    THE unified terminal. One Claude Code terminal, one endpoint, everything behind it:

      Claude Code  ->  unified_proxy (:8770)  ->  Ollama (:11434, LOCAL ONLY)
                          injects memory + a sanitized, uncensored skill every turn
                          routes coder <-> thinker per message marker (@local / @think)

    You type in one place. Every turn auto-gets the relevant project facts and, when
    the task matches one, a sanitized skill (refusal/disclaimer lines stripped, so the
    abliterated model is never re-censored). Route a single turn to the reasoning model
    by putting @think in the message; @local forces the coder. Default is local (free,
    offline, uncensored). Cloud was removed - both engines are local.

    This works under `claude --bare` (hooks/plugins off for the 32k window) because the
    proxy injects at the HTTP layer, not via a hook.

    Usage:  .\tools\oneclaude.ps1
            .\tools\oneclaude.ps1 -Default think          # default route = the reasoning model
            .\tools\oneclaude.ps1 -Model uncensored-coder-q6
            .\tools\oneclaude.ps1 -NoMcp -p "one-shot prompt"
#>
param(
    [string]$Model   = "uncensored-coder",
    [ValidateSet("local", "think")][string]$Default = "local",
    [string]$Effort  = "low",
    [switch]$WithMcp   # curated MCP is OFF by default: the local model overflows on their tool
)                      # schemas (measured: "hi" alone = 24208 tokens WITH MCP, over the 24000
                       # harness budget; with no MCP it fits and answers).

$ErrorActionPreference = "Stop"
$repo = Split-Path $PSScriptRoot -Parent
$proxyPort = 8770

# -- 1. Ollama server up. Set the E: model store AND the KV-cache config BEFORE `ollama serve`
#       starts, so the serve process inherits them. The coder ships num_ctx 32768, which
#       Modelfile.uncensored-coder says REQUIRES a q8_0 KV cache to fit the 16GB card (else it
#       goes ~45GB resident and collapses to ~5 tok/s). Relying on the persisted User-scope vars
#       is fragile - set them here so a fresh machine / cleared env cannot silently degrade.
$env:OLLAMA_MODELS         = "E:\ollama-models"
$env:OLLAMA_KV_CACHE_TYPE  = "q8_0"
$env:OLLAMA_FLASH_ATTENTION = "1"
$env:OLLAMA_NUM_PARALLEL   = "1"        # one GPU, one 18GB model: serialise (else N KV slots)
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
    $env:PYTHONIOENCODING    = "utf-8"
    $env:UNIFIED_DEFAULT     = $Default
    $env:UNIFIED_LOCAL_MODEL = $Model
    Start-Process -FilePath "python" -ArgumentList "tools\local_proxy.py" `
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
# Tell Claude Code the model's REAL window so it auto-compacts BEFORE overflowing.
# The model's num_ctx is 32768. This is the window Claude Code manages BEFORE it hard-stops
# with "context limit reached" (its own limit - it hits this when a single large tool result,
# e.g. a broad grep, will not fit and it cannot compact one item). Set it as HIGH as the model
# can back: the proxy now reliably trims any model-side overflow (fit_window bounds input +
# counts the tools schema, cap_output_tokens bounds max_tokens), so Claude Code can safely hold
# more before hard-stopping - the proxy trims the ~few-k excess for the model. 30000 lets a
# ~30k-token search result through where 24000 hard-stopped; a truly huge result (40k+) still
# needs a scoped task. Do NOT exceed the model window much, or the proxy trims context CC thinks
# it still has.
$env:CLAUDE_CODE_MAX_CONTEXT_TOKENS = "30000"
# THE FIX for "Context limit reached" on a tiny first tool call. Claude Code does not
# recognise the model id "uncensored-coder", so it ASSUMES a context window and ENFORCES it
# client-side, hard-stopping before it ever calls the model. Its own message (extracted from
# claude.exe) names this escape hatch verbatim: the flag "restores the previous
# wait-for-the-API behavior". That is exactly what we want here, because the proxy already
# guarantees the request fits the real window (fit_window bounds input incl. the tools schema,
# cap_output_tokens bounds max_tokens), so the API cannot return a context error for CC to wait
# on. Without this, CC's assumed window fights the real 32768 and stops work that would succeed.
# NOTE: modelOverrides is NOT the lever - in this build it maps an Anthropic model ID to a
# provider-specific ID (Bedrock ARNs); it carries no contextWindow field.
$env:CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT = "1"

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

$mcpLabel = if ($WithMcp) { "rust,potassium,memory,filesystem" } else { "none (local ctx cannot hold MCP schemas)" }
Write-Host ""
Write-Host "  UNIFIED TERMINAL" -ForegroundColor Green
Write-Host "  engines (LOCAL ONLY, both uncensored): @local=$Model (codes)  @think=uncensored-thinker (reasons)" -ForegroundColor Green
Write-Host "  every turn: memory + vault + sanitized skill injected via :$proxyPort; Claude-agent identity stripped" -ForegroundColor DarkGray
Write-Host "  route one turn: put @local or @think in the message (default $Default) - no cloud" -ForegroundColor DarkGray
Write-Host "  coder ctx=32768  thinker ctx=16384  effort=$Effort  mcp=$mcpLabel  (bare: no hooks/plugins)" -ForegroundColor DarkGray
Write-Host ""

& "C:\Users\jep\.local\bin\claude.exe" `
    --bare `
    --strict-mcp-config `
    --mcp-config $mcpFile `
    --settings $settingsFile `
    @args
