@echo off
chcp 65001 >nul
cd /d "%~dp0"
title 快传
where npm >nul 2>&1
if errorlevel 1 (
  echo 没有找到 npm，请先安装 Node.js
  pause
  exit /b 1
)
where cargo >nul 2>&1
if errorlevel 1 (
  echo 没有找到 cargo，请先安装 Rust
  echo https://rustup.rs
  pause
  exit /b 1
)
if not exist node_modules (
  echo 第一次运行，正在安装界面依赖...
  call npm install
  if errorlevel 1 (
    echo 安装失败
    pause
    exit /b 1
  )
)
call npm run tauri dev
if errorlevel 1 pause
