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

# THERMAL + RESOURCE GUARD. Jep is abroad and long GPU benchmarks run unattended, so the
# watchdog also records temps and steps in if the box is genuinely in trouble. Baseline
# measured 26 Aug under 86% GPU load: 5060 Ti 49C, 14.8W of 180W, fan 32%, no throttling.
# The 5060 Ti throttles around 83C, so 85C is a real emergency rather than a busy afternoon.
try {
  $q = & nvidia-smi --query-gpu=index,temperature.gpu,utilization.gpu,clocks_throttle_reasons.hw_thermal_slowdown --format=csv,noheader 2>$null
  foreach ($line in $q) {
    if ($line -match '^\s*(\d+),\s*(\d+),\s*(\d+)') {
      $idx = $Matches[1]; $temp = [int]$Matches[2]; $util = $Matches[3]
      if ($temp -ge 85) {
        L "THERMAL EMERGENCY: GPU $idx at ${temp}C - unloading models to let it cool"
        & "$env:LOCALAPPDATA\Programs\Ollama\ollama.exe" stop uncensored-coder 2>$null
        & "$env:LOCALAPPDATA\Programs\Ollama\ollama.exe" stop uncensored-thinker 2>$null
      } elseif ($temp -ge 78) {
        L "WARNING: GPU $idx at ${temp}C (util ${util}%) - hot but under the 83C throttle point"
      }
    }
  }
  $os = Get-CimInstance Win32_OperatingSystem
  $freeGB = [math]::Round($os.FreePhysicalMemory/1MB, 1)
  if ($freeGB -lt 2) { L "WARNING: only ${freeGB}GB RAM free - a runaway llama-server is the usual cause" }
} catch { }

# UNCENSORED GUARD. An abliterated model refusing legitimate reverse-engineering work is a
# SYMPTOM, not a judgement: it means the alias no longer points at the abliterated weights, or
# the Modelfile's uncensored SYSTEM prompt was lost (wrong base, an ollama pull, a bad edit).
# tools/uncensored_guard.py probes with gate G5's own prompts and markers and, on a refusal,
# REINSTALLS the alias from the Modelfile and re-verifies.
# Rate-limited to hourly: the probes cost real model time and this runs every 5 minutes.
# Skipped entirely while a benchmark holds the server, so it can never contaminate a gate run.
try {
  $stamp = Join-Path $PSScriptRoot ".uncensored_guard.stamp"
  $due = $true
  if (Test-Path $stamp) { $due = ((Get-Date) - (Get-Item $stamp).LastWriteTime).TotalMinutes -ge 60 }
  $benchBusy = @(Get-CimInstance Win32_Process -Filter "Name='python.exe'" -EA 0 |
                 Where-Object { $_.CommandLine -match 'goal_gates|bench_process|bisect_prompt' }).Count -gt 0
  if ($due -and -not $benchBusy) {
    Set-Content -Path $stamp -Value (Get-Date).ToString("s") -Encoding ascii
    $out = & python (Join-Path $PSScriptRoot "uncensored_guard.py") 2>&1 | Out-String
    $code = $LASTEXITCODE
    if ($code -eq 1) { L "UNCENSORED GUARD: refusal detected, alias REINSTALLED and verified" }
    elseif ($code -ge 2) { L "UNCENSORED GUARD: STILL REFUSING after reinstall - needs a human" }
    # exit 0 is the normal case and is intentionally not logged, to keep the log readable
  }
} catch { L ("uncensored guard error: " + $_.Exception.Message) }
