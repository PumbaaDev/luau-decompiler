@echo off
REM Self-sufficient phone-stack launcher: ensures Ollama is running, then keeps phone_agent alive.
cd /d C:\Users\jep\Downloads\luau-decompiler-v2
set OLLAMA_MODELS=E:\ollama-models
set OLLAMA_KV_CACHE_TYPE=q8_0
set OLLAMA_FLASH_ATTENTION=1
set OLLAMA_NUM_PARALLEL=1
set OLLAMA_KEEP_ALIVE=30m
set PYTHONIOENCODING=utf-8
REM start the Ollama server if it isn't already running
tasklist /fi "imagename eq ollama.exe" | find /i "ollama.exe" >nul
if errorlevel 1 (
  start "" /b "%LOCALAPPDATA%\Programs\Ollama\ollama.exe" serve
  timeout /t 8 /nobreak >nul
)
:loop
python tools\phone_agent.py
timeout /t 5 /nobreak >nul
goto loop
