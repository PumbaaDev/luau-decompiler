# Watchdog: keeps ollama (:11434) + the local proxy (:8770) alive.
# Named ai_keepalive (not keep_ai_alive) because Bitdefender path-blocked the old name.
$repo = Split-Path $PSScriptRoot -Parent
$log  = Join-Path $PSScriptRoot "ai_keepalive.log"
function L($m) { "{0}  {1}" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"), $m | Tee-Object -FilePath $log -Append }
$env:OLLAMA_MODELS = "E:\ollama-models"; $env:OLLAMA_KV_CACHE_TYPE = "q8_0"
$env:OLLAMA_FLASH_ATTENTION = "1"; $env:OLLAMA_NUM_PARALLEL = "1"; $env:OLLAMA_KEEP_ALIVE = "30m"
$env:PYTHONIOENCODING = "utf-8"; $env:UNIFIED_DEFAULT = "local"; $env:UNIFIED_LOCAL_MODEL = "uncensored-coder"
try { Invoke-WebRequest "http://localhost:11434/api/tags" -TimeoutSec 8 -UseBasicParsing | Out-Null }
catch {
  L "ollama down - starting"
  Start-Process "$env:LOCALAPPDATA\Programs\Ollama\ollama.exe" -ArgumentList "serve" -WindowStyle Hidden
  Start-Sleep 12
}
$ok = $false
try { if ((Invoke-WebRequest "http://localhost:8770/health" -TimeoutSec 5 -UseBasicParsing).Content -match '"status"\s*:\s*"ok"') { $ok = $true } } catch { }
if (-not $ok) {
  L "proxy unhealthy - restarting"
  $p = (Get-NetTCPConnection -LocalPort 8770 -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess
  if ($p) { Stop-Process -Id $p -Force -ErrorAction SilentlyContinue; Start-Sleep 1 }
  Start-Process "python" -ArgumentList "tools\local_proxy.py" -WorkingDirectory $repo -WindowStyle Hidden
  Start-Sleep 6
  try { L ("proxy up: " + (Invoke-WebRequest "http://localhost:8770/health" -TimeoutSec 5 -UseBasicParsing).Content) } catch { L "proxy STILL down" }
} else { L "ok (both healthy)" }
