#!/bin/sh
# Cross-build the Windows executables from Linux/macOS.
#   apt install gcc-mingw-w64-x86-64-posix && rustup target add x86_64-pc-windows-gnu
# On Windows itself, just: cargo build --release
set -e
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_AR=x86_64-w64-mingw32-ar
cargo build --release --target x86_64-pc-windows-gnu -j 1
mkdir -p dist
cp target/x86_64-pc-windows-gnu/release/fsearch.exe target/x86_64-pc-windows-gnu/release/fsearchd.exe dist/
echo "built dist/fsearch.exe (console CLI) and dist/fsearchd.exe (windowless daemon)"
