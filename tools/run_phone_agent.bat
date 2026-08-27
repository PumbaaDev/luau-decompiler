@echo off
cd /d C:\Users\jep\Downloads\luau-decompiler-v2
set OLLAMA_MODELS=E:\ollama-models
set PYTHONIOENCODING=utf-8
:loop
python tools\phone_agent.py
timeout /t 5 /nobreak >nul
goto loop
