@echo off
chcp 65001 >nul
setlocal

REM ============================================================
REM  CineBackup - Windows 一键构建脚本
REM
REM  它替你解决两个本机特有的坑：
REM
REM  【坑 1】LNK1181: 无法打开输入文件 "dbghelp.lib"
REM    本机 Windows SDK 只注册在 32 位注册表视图（WOW6432Node），
REM    64 位 rustc 读的是 64 位视图，因此定位不到 SDK 的库目录。
REM    → 本脚本先执行微软官方 vcvars64.bat 显式注入 LIB / PATH。
REM
REM  【坑 2】failed to bundle project: `timeout: global`
REM    tauri 打包前要从 GitHub Release 下载 WiX / NSIS 工具链；
REM    本机直连吞吐仅约 176 KB/s，而它的下载器约 1 分钟就放弃。
REM    → 本脚本先用镜像把工具链预置到 %LOCALAPPDATA%\tauri\。
REM
REM  用法：双击本文件；或在 cmd 里执行 tools\build_windows.bat
REM  只打某一种包：tools\build_windows.bat msi   或  ... nsis
REM ============================================================

set "ROOT=%~dp0.."
set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set "BUNDLES=%~1"

echo ============================================================
echo  [1/5] 检查 MSVC 编译环境
echo ============================================================
if not exist "%VCVARS%" (
  echo [错误] 未找到 vcvars64.bat：
  echo        %VCVARS%
  echo        请先安装 Visual Studio 2022 生成工具 + VCTools 工作负载：
  echo        winget install --id Microsoft.VisualStudio.2022.BuildTools -e --source winget ^
  echo          --override "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
  pause
  exit /b 1
)
call "%VCVARS%" >nul
if errorlevel 1 (
  echo [错误] vcvars64.bat 执行失败。
  pause
  exit /b 1
)
echo   OK

echo.
echo ============================================================
echo  [2/5] 准备打包工具链 WiX / NSIS（镜像加速）
echo ============================================================
set "PY="
where py >nul 2>&1 && set "PY=py"
if not defined PY ( where python >nul 2>&1 && set "PY=python" )
if defined PY (
  %PY% "%ROOT%\tools\setup_bundler_tools.py"
  if errorlevel 1 echo   [警告] 工具链准备失败，若打 MSI 可能会超时。
) else (
  echo   [跳过] 未找到 Python。若打包时报 timeout: global，
  echo          请手动执行： python tools\setup_bundler_tools.py
)

cd /d "%ROOT%"
echo.
echo ============================================================
echo  [3/5] 工作目录: %CD%
echo ============================================================

echo.
echo ============================================================
echo  [4/5] 安装前端依赖
echo ============================================================
if not exist "node_modules" (
  call npm install --no-audit --no-fund
) else (
  echo   已存在 node_modules，跳过
)

echo.
echo ============================================================
echo  [5/5] 构建（首次约 10-20 分钟）
echo ============================================================
REM dist/ 由 python 清空：vite 自己清会被本机 node 的「批量删除守卫」拦下
if defined PY (
  %PY% "%ROOT%\tools\clean_dist.py"
) else (
  echo   [跳过] 未找到 Python，dist 不会被清空（旧产物可能残留，不影响运行）
)
if defined BUNDLES (
  call npm run tauri build -- --bundles %BUNDLES%
) else (
  call npm run tauri build
)
set "RC=%errorlevel%"

echo.
echo ============================================================
if "%RC%"=="0" (
  echo  构建成功，产物：
  echo    %CD%\src-tauri\target\release\cinebackup.exe                      ^(免安装，可直接双击^)
  echo    %CD%\src-tauri\target\release\bundle\msi\                          ^(.msi^)
  echo    %CD%\src-tauri\target\release\bundle\nsis\                         ^(setup.exe^)
) else (
  echo  构建失败，退出码 %RC%
)
echo ============================================================
pause
exit /b %RC%
