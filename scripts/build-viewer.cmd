@echo off
rem ── fusion-viewer 构建包装 ──
rem 系统 PATH 中的 msys2/mingw64 会让 cc crate/bindgen 误用 mingw 头文件（与 MSVC 冲突），
rem 而 cargo 的 [env] 不支持覆盖 PATH，因此构建时用干净 PATH 运行 cargo。
rem 用法：scripts\build-viewer.cmd check|test|build [--release]
set "PATH=C:\Windows\System32;C:\Windows;C:\Program Files\Git\cmd"
cargo %*
