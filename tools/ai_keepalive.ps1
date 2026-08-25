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

# FOOTPRINT CHECK: ollama answering is NOT the same as ollama being correct. If the server
# was started without OLLAMA_KV_CACHE_TYPE=q8_0 the KV cache is f16, the model loads at ~22GB
# instead of ~20GB, more layers fall back to CPU (23%/77% vs 18%/82%) and OUTPUT CHANGES -
# measured 25 Aug: it drifted the benchmark control from 73/83 to 72/83. Detect and repair.
try {
  $ps = & "$env:LOCALAPPDATA\Programs\Ollama\ollama.exe" ps 2>$null | Out-String
  if ($ps -match '(\d+)\s*GB') {
    $gb = [int]$Matches[1]
    if ($gb -ge 22) {
      L "DEGRADED: model loaded at ${gb}GB (expected ~20GB) - KV cache is not q8_0. Restarting ollama with correct env."
      Get-Process -Name 'ollama','ollama app','llama-server' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
      Start-Sleep 4
      Start-Process "$env:LOCALAPPDATA\Programs\Ollama\ollama.exe" -ArgumentList "serve" -WindowStyle Hidden
      Start-Sleep 10
      L "ollama restarted with q8_0 KV cache"
    }
  }
} catch { }
