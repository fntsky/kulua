@echo off
rem fusion-viewer build wrapper
rem msys2/mingw64 in PATH makes cc/bindgen pick mingw headers (conflicts with MSVC),
rem and cargo [env] cannot override PATH, so build with a clean PATH here.
rem usage: scripts\build-viewer.cmd check|test|build [--release]
set "PATH=C:\Windows\System32;C:\Windows;C:\Program Files\Git\cmd;C:\Users\huan\.cargo\bin"
cargo %*
